//! Eval and dynamic function constructor opcode helpers.
//!
//! `eval` and `new Function(...)` recurse through the VM compiler/runtime path,
//! so their dispatch has to run before the dense in-frame match borrows the
//! current frame.
//!
//! # Contents
//! - Indirect eval execution and writeback.
//! - `Function` constructor argument coercion and body synthesis.
//!
//! # Invariants
//! - Helpers advance the current frame PC exactly once on success.
//! - Compiled eval / `new Function` modules link into the
//!   interpreter's code space, so escaping closures and classes keep
//!   resolvable global function ids.
//! - Per-argument coercion re-reads each value from its GC-visited
//!   slot (frame register / native argument storage) because user
//!   `toString` can move the heap.
//! - Strict-mode eval inherits the caller function strictness.
//! - Direct eval re-enters above an activation floor on the caller's stack;
//!   caller frames remain traced and cannot be consumed by nested dispatch.
//! - Direct eval compiles against the caller's context chain
//!   ([`otter_bytecode::EvalCallerChain`]) and runs its `<main>` as a SELF
//!   closure over the caller's innermost context, read back from its traced
//!   register after compilation and linking. Bindings a sloppy eval creates
//!   land in the var-scope context's eval extension through the body's own
//!   `DeclareEvalVar`; the runtime adopts nothing.
//! - Every other compiled `<main>` (indirect eval, host scripts, `Function`,
//!   the CommonJS wrapper) runs as a SELF closure over no context.
//!
//! # See also
//! - [`crate::code_space`]
//! - [`crate::ExecutionContext`]

use crate::activation_stack::ActivationStack;
use otter_bytecode::{BytecodeModule, Operand};
use smallvec::SmallVec;

use crate::promise::JsPromise;
use crate::{
    AsyncFrameState, EvalCompileOptions, ExecutionContext, Interpreter, Value, VmError,
    abstract_ops, operand_decode::register_operand, promise_dispatch, read_register,
    write_register,
};

/// §20.2.1.1.1 CreateDynamicFunction `kind` parameter: which function
/// goal symbol the synthesised source compiles under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DynamicFunctionKind {
    /// `Function(...)` — `normal`.
    Normal,
    /// `%GeneratorFunction%(...)` — `generator`.
    Generator,
    /// `%AsyncFunction%(...)` — `async`.
    Async,
    /// `%AsyncGeneratorFunction%(...)` — `async-generator`.
    AsyncGenerator,
}

impl DynamicFunctionKind {
    pub(crate) fn source_prefix(self) -> &'static str {
        match self {
            Self::Normal => "function",
            Self::Generator => "function*",
            Self::Async => "async function",
            Self::AsyncGenerator => "async function*",
        }
    }
}

impl Interpreter {
    /// `Eval dst, src, ctx, flags` — §19.2.1.1 PerformEval with
    /// `direct = true`.
    pub(crate) fn run_eval_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        operands: impl crate::executable::OperandSource,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let src_reg = register_operand(operands.get(1))?;
        let ctx_reg = register_operand(operands.get(2))?;
        let flags = match operands.get(3) {
            Some(Operand::Imm32(bits)) => bits,
            _ => return Err(VmError::InvalidOperand),
        };
        let forbid_var_arguments = flags & 1 != 0;
        let new_target_allowed = flags & 4 != 0;
        let in_class_field_initializer = flags & 8 != 0;
        let super_property_allowed = flags & 16 != 0;
        let super_call_allowed = flags & 32 != 0;
        let top_idx = stack.len() - 1;
        let value = *read_register(&stack[top_idx], src_reg)?;
        if let Some(s) = value.as_string(&self.gc_heap) {
            let source = s.to_lossy_string(&self.gc_heap);
            if is_v8_native_eval_hint(&source) {
                let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                write_register(frame, dst, Value::undefined())?;
                frame.advance_pc()?;
                return Ok(());
            }
        }
        let result = if value.as_string(&self.gc_heap).is_some() {
            let force_strict = context.function_is_strict(stack[top_idx].function_id);
            let caller_context = *read_register(&stack[top_idx], ctx_reg)?;
            let caller_chain = self.eval_caller_chain(context, caller_context)?;
            self.run_direct_eval(
                &value,
                EvalCompileOptions {
                    force_strict,
                    forbid_var_arguments,
                    caller_chain: Some(caller_chain),
                    script_goal: false,
                    new_target_allowed,
                    in_class_field_initializer,
                    super_property_allowed,
                    super_call_allowed,
                    function_constructor: false,
                },
                ctx_reg,
                in_class_field_initializer,
                stack,
            )?
        } else {
            // §19.2.1.1 step 2 — a non-String operand is the result.
            value
        };
        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// The SELF closure a compiled `<main>` runs as, over `context` (the
    /// caller's innermost context for a direct eval, `undefined` otherwise).
    fn main_self_closure(&mut self, main_id: u32, context: Value) -> Result<Value, VmError> {
        // The context rides in the pending closure body.
        let closure =
            crate::closure::alloc_closure(&mut self.gc_heap, main_id, context, None, None)
                .map_err(crate::oom_to_vm)?;
        Ok(Value::closure(closure))
    }

    /// Execute a direct eval (§19.2.1.1 PerformEval with `direct = true`).
    ///
    /// The body was compiled against the caller's context chain; its
    /// `<main>` runs as a closure over the caller's innermost context with
    /// the caller's `this` and `new.target`. Declarations reach the caller's
    /// scopes through the body's own context operations.
    fn run_direct_eval(
        &mut self,
        value: &Value,
        options: EvalCompileOptions,
        ctx_reg: u16,
        new_target_suppressed: bool,
        stack: &mut ActivationStack,
    ) -> Result<Value, VmError> {
        let Some(s) = value.as_string(&self.gc_heap) else {
            // §19.2.1.1 step 2 — non-string operands are returned
            // unchanged.
            return Ok(*value);
        };
        let source = s.with_utf16(&self.gc_heap, crate::eval_source::encode);
        let top_idx = stack.len().checked_sub(1).ok_or(VmError::InvalidOperand)?;
        let module = self.compile_escaped_source(&source, options)?;
        let context = self
            .link_evictable_module(module)
            .map_err(|_| VmError::InvalidOperand)?;
        let main = context.exec_main();
        // Linking can collect: the caller context is read back from its
        // traced register only now.
        let caller_context = *read_register(&stack[top_idx], ctx_reg)?;
        crate::context_ops::context_operand(caller_context)?;
        let self_value = self.main_self_closure(main.id, caller_context)?;
        let entry_this = stack[top_idx].this_value;
        // §13.3.3 — `new.target` in the eval body reads the caller
        // frame's value (direct eval is contained in function code).
        // Class field initializers observe `undefined` (§15.7.10).
        let caller_new_target = if new_target_suppressed {
            Value::undefined()
        } else {
            stack[top_idx].new_target()
        };
        let mut entry = crate::PreparedCall::for_code_block(main, None, self_value, entry_this);
        entry.set_new_target(caller_new_target);
        // Direct eval is synchronous re-entry in the caller's logical
        // activation chain. Keep the caller frames published for GC, stack
        // traces, and exception diagnostics, while the floor prevents the
        // nested dispatch from executing or consuming them.
        let floor = stack.floor();
        stack.push(entry);
        let result = self.dispatch_loop_above_rooted(&context, stack, floor);

        // Successful return and throw-unwind normally consume the complete
        // nested region themselves. Non-language VM failures may leave one or
        // more materialized eval frames behind; release their cold records and
        // register windows in strict LIFO order before returning to the caller.
        self.release_frames_above(stack, floor);
        result
    }

    pub(crate) fn run_new_function_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        operands: impl crate::executable::OperandSource,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let argc = match operands.get(1) {
            Some(Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        // Coerce one argument at a time, re-reading each value from
        // its frame register right before coercion: user `toString`
        // can trigger a moving collection, and registers are the
        // GC-traced (and rewritten) home of these values — a Rust-side
        // snapshot of the whole argument list would go stale.
        let mut parts: Vec<String> = Vec::with_capacity(argc);
        for i in 0..argc {
            let r = register_operand(operands.get(2 + i))?;
            let top_idx = stack.len() - 1;
            let value = *read_register(&stack[top_idx], r)?;
            parts.push(self.function_constructor_arg_to_string(stack, context, &value)?);
        }
        let result = self.build_function_constructor_from_parts(stack, parts)?;
        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Execute `source` as an ECMAScript *Script* in the current
    /// realm (§16.1.6 ScriptEvaluation) — the host API behind
    /// `$262.evalScript`. Differs from indirect eval only in GDI
    /// semantics: global var bindings are non-configurable.
    ///
    /// # Errors
    /// - [`VmError::SyntaxError`] when parsing / compilation fail.
    pub fn run_host_script(&mut self, source: &Value) -> Result<Value, VmError> {
        crate::NativeCtx::with_host_context(
            self,
            crate::NativeCallInfo::default_call(),
            None,
            |ctx| {
                ctx.with_turn_parts(|interp, stack| {
                    interp.run_eval(
                        stack,
                        source,
                        EvalCompileOptions {
                            script_goal: true,
                            ..Default::default()
                        },
                    )
                })
            },
        )
    }

    /// Execute `eval(source)` per §19.4.1.1 indirect-eval semantics:
    /// parse + compile via the embedder hook, then run `<main>` above an
    /// activation floor on the current rooted runtime turn. Caller-owned frames
    /// remain in place and are released only down to that floor.
    ///
    /// # Errors
    /// - [`VmError::SyntaxError`] when no eval hook is installed or
    ///   parsing / compilation fail.
    pub(crate) fn run_eval(
        &mut self,
        stack: &mut ActivationStack,
        value: &Value,
        options: EvalCompileOptions,
    ) -> Result<Value, VmError> {
        let Some(s) = value.as_string(&self.gc_heap) else {
            // Per §19.4.1.1 step 4, eval'd non-strings are returned
            // unchanged — `eval(42) === 42`.
            return Ok(*value);
        };
        let source = s.with_utf16(&self.gc_heap, crate::eval_source::encode);
        if is_v8_native_eval_hint(&source) {
            return Ok(Value::undefined());
        }
        let module = self.compile_escaped_source(&source, options)?;
        // Linking (not a standalone context) keeps the eval chunk's
        // function ids global, so closures and classes escaping the
        // eval stay callable from any later frame.
        let context = self
            .link_evictable_module(module)
            .map_err(|_| VmError::InvalidOperand)?;
        let main = context.exec_main();
        let self_value = self.main_self_closure(main.id, Value::undefined())?;
        // §19.2.1.3 — eval code evaluated at global scope (direct at
        // the top level or indirect) binds `this` to globalThis even
        // when the eval source itself is strict; only module code gets
        // an undefined top-level `this`.
        let entry_this = if main.is_module {
            Value::undefined()
        } else {
            Value::object(self.global_this)
        };
        let entry = crate::PreparedCall::for_code_block(main, None, self_value, entry_this);
        let entry_is_async = main.is_async;
        let floor = stack.floor();
        stack.push(entry);
        self.with_handle_scope(|interp, scope| {
            let entry_promise = if entry_is_async {
                let result = promise_dispatch::PromiseBuilder::with_context(context.clone())
                    .pending_stack_rooted(interp, stack, &[], &[])?;
                let frame = stack.pending_mut().expect("entry inputs were just queued");
                interp.prepared_set_async_state(
                    frame,
                    AsyncFrameState {
                        result_promise: result,
                    },
                );
                Some(interp.scoped_value(scope, Value::promise(result)))
            } else {
                None
            };
            let result = interp.dispatch_loop_above_rooted(&context, stack, floor);
            interp.release_frames_above(stack, floor);
            let value = result?;
            if let Some(promise) = entry_promise {
                // Drain microtasks attached to top-level await so the
                // entry promise settles before we read its value.
                interp
                    .drain_microtasks_with_default(Some(context))
                    .map_err(|e| e.error)?;
                let promise = interp
                    .escape_scoped(promise)
                    .as_promise()
                    .ok_or(VmError::TypeMismatch)?;
                return Ok(match promise.state(&interp.gc_heap) {
                    crate::promise::PromiseState::Fulfilled(v) => v,
                    crate::promise::PromiseState::Rejected(reason) => {
                        return Err(interp.err_uncaught((interp.render_thrown(&reason)).into()));
                    }
                    crate::promise::PromiseState::Pending => Value::undefined(),
                });
            }
            Ok(value)
        })
    }

    /// Build a `Function(args, body)` callable per §20.2.1.1. `args`
    /// must live in GC-visited slots (native-call argument storage or
    /// frame registers) because per-argument coercion can re-enter
    /// user code and move the heap; each iteration re-reads its slot.
    /// The synthesised module links into the interpreter's code space,
    /// so the returned closure's function id resolves from any frame —
    /// no wrapper indirection is needed.
    /// §20.2.1.1.1 CreateDynamicFunction over native-call arguments,
    /// parameterised by function `kind` so `%GeneratorFunction%`,
    /// `%AsyncFunction%`, and `%AsyncGeneratorFunction%` compile their
    /// bodies under the right goal symbol.
    pub(crate) fn build_dynamic_function(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        args: &[Value],
        kind: DynamicFunctionKind,
    ) -> Result<Value, VmError> {
        self.with_handle_scope(|interp, scope| {
            let arg_handles: SmallVec<[crate::Local<'_>; 8]> = args
                .iter()
                .copied()
                .map(|value| interp.scoped_value(scope, value))
                .collect();
            // Coercion can re-enter user code. Re-read each argument from its
            // moving-GC handle so an earlier conversion cannot stale later
            // constructor arguments.
            let mut parts: Vec<String> = Vec::with_capacity(arg_handles.len());
            for handle in arg_handles {
                let arg = interp.escape_scoped(handle);
                parts.push(interp.function_constructor_arg_to_string(stack, context, &arg)?);
            }
            interp.build_dynamic_function_from_parts(stack, parts, kind)
        })
    }

    /// §20.2.1.1 steps 2+ over already-coerced argument strings.
    pub(crate) fn build_function_constructor_from_parts(
        &mut self,
        stack: &mut ActivationStack,
        parts: Vec<String>,
    ) -> Result<Value, VmError> {
        self.build_dynamic_function_from_parts(stack, parts, DynamicFunctionKind::Normal)
    }

    /// Build a CommonJS module wrapper function and return it as a callable
    /// value:
    ///
    /// ```text
    /// (function anonymous(exports, require, module, __filename, __dirname) {
    ///   <body>
    /// })
    /// ```
    ///
    /// Reentry-safe: like `new Function`, the synthesised body links into the
    /// interpreter's code space (it does NOT go through [`Interpreter::run`],
    /// which swaps `code_space` and is unsafe to call nested), so the returned
    /// closure can be created from inside a native call and invoked through
    /// [`Interpreter::run_callable_sync`]. Used by the runtime CommonJS loader
    /// to execute `require`d modules.
    ///
    /// # Errors
    /// Returns a `VmError` if the body fails to compile (surfaced as a
    /// `SyntaxError`) or if the eval/compiler hook is not installed.
    pub fn create_commonjs_wrapper(
        &mut self,
        stack: &mut ActivationStack,
        module_url: &str,
        body: &str,
    ) -> Result<Value, VmError> {
        // Node-style wrapper with the entire prologue on line 1, so a
        // source line `N` maps to wrapped line `N`: stack-trace line
        // numbers match the original file (only line-1 columns carry the
        // prologue offset — the same quirk Node has).
        let source =
            format!("(function (exports, require, module, __filename, __dirname) {{ {body}\n}})");
        let mut module = self.compile_eval_source(&source, EvalCompileOptions::default())?;
        // Stamp the synthesized module + its functions with the file URL
        // so frames captured for `Error.prototype.stack` report the file
        // rather than the synthetic eval name.
        module.module = module_url.to_string();
        for function in &mut module.functions {
            function.module_url = module_url.to_string();
        }
        // Register the wrapped source so frame spans resolve to
        // `(line, column)` against it. The wrapper is VM-synthesized, so it
        // is admitted against the registry's source budget here.
        self.register_module_source_owned(module_url.to_string(), source)
            .map_err(|_| VmError::InvalidOperand)?;
        let context = self
            .link_evictable_module(module)
            .map_err(|_| VmError::InvalidOperand)?;
        // Running the synthesised module's `<main>` returns the wrapper
        // function value (the parenthesised expression is the program's
        // completion).
        let main = context.exec_main();
        let self_value = self.main_self_closure(main.id, Value::undefined())?;
        let floor = stack.floor();
        stack.push(crate::PreparedCall::for_code_block(
            main,
            None,
            self_value,
            Value::undefined(),
        ));
        let result = self.dispatch_loop_above_rooted(&context, stack, floor);
        self.release_frames_above(stack, floor);
        result
    }

    /// §20.2.1.1.1 CreateDynamicFunction steps 7–20: synthesise the
    /// `kind`-prefixed source text, compile through the eval hook, and
    /// return the resulting function value.
    pub(crate) fn build_dynamic_function_from_parts(
        &mut self,
        stack: &mut ActivationStack,
        parts: Vec<String>,
        kind: DynamicFunctionKind,
    ) -> Result<Value, VmError> {
        let (params, body): (Vec<&str>, &str) = if parts.is_empty() {
            (Vec::new(), "")
        } else {
            let body = parts.last().expect("non-empty checked above").as_str();
            let params: Vec<&str> = parts[..parts.len() - 1]
                .iter()
                .map(String::as_str)
                .collect();
            (params, body)
        };
        let params_joined = params.join(",");
        let prefix = kind.source_prefix();
        // §20.2.1.1.1 steps 20-21 — the parameter text and the body text
        // are each parsed on their own before the assembled source is.
        // Without that, a parameter list that swallows the generated
        // `) {` (an unterminated comment, an open template literal, a
        // nested `function (`) assembles into a program that parses,
        // and the caller smuggles source past the parameter list.
        let probe_options = EvalCompileOptions {
            function_constructor: true,
            ..EvalCompileOptions::default()
        };
        let params_probe = format!("({prefix} anonymous({params_joined}\n) {{\n\n}})");
        self.compile_escaped_source(&params_probe, probe_options.clone())?;
        let body_probe = format!("({prefix} anonymous(\n) {{\n{body}\n}})");
        self.compile_escaped_source(&body_probe, probe_options)?;
        let source = format!("({prefix} anonymous({params_joined}\n) {{\n{body}\n}})");
        let module = self.compile_escaped_source(
            &source,
            EvalCompileOptions {
                function_constructor: true,
                ..EvalCompileOptions::default()
            },
        )?;
        let context = self
            .link_evictable_module(module)
            .map_err(|_| VmError::InvalidOperand)?;
        // Running the synthesised module's `<main>` returns the
        // function value (the parenthesised expression is the
        // program's completion).
        let main = context.exec_main();
        let self_value = self.main_self_closure(main.id, Value::undefined())?;
        let floor = stack.floor();
        stack.push(crate::PreparedCall::for_code_block(
            main,
            None,
            self_value,
            Value::undefined(),
        ));
        let result = self.dispatch_loop_above_rooted(&context, stack, floor);
        self.release_frames_above(stack, floor);
        result
    }

    fn function_constructor_arg_to_string(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        value: &Value,
    ) -> Result<String, VmError> {
        let primitive = if value.is_object() || value.is_proxy() {
            self.to_primitive_string_hint_sync(stack, context, *value)?
        } else {
            *value
        };
        if let Some(s) = primitive.as_string(&self.gc_heap) {
            // Escaped here rather than after assembly: the surrounding source
            // the caller builds is ASCII punctuation, so escaping each part
            // leaves one consistently escaped program.
            return Ok(s.with_utf16(&self.gc_heap, crate::eval_source::encode));
        }
        if primitive.is_symbol() {
            return Err(
                self.err_type(("Cannot convert a Symbol value to a string".to_string()).into())
            );
        }
        Ok(primitive.display_string(&self.gc_heap))
    }

    // `to_*` mirrors the spec abstract operation `ToPrimitive` (§7.1.1).
    // The interpreter borrow is `&mut self` because the helper invokes
    // user-defined `toString` / `valueOf`, which can re-enter dispatch.
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn to_primitive_string_hint_sync(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<Value, VmError> {
        // §7.1.1 ToPrimitive with hint "string" — must first consult the
        // `@@toPrimitive` method, then fall back to the
        // OrdinaryToPrimitive `toString` / `valueOf` ladder. Delegate to
        // the full implementation so both paths agree.
        self.to_primitive_sync(stack, context, value, abstract_ops::ToPrimitiveHint::String)
    }

    /// Helper — invoke the eval hook, mapping its error to a
    /// VmError that the throwable-conversion path will surface as
    /// `SyntaxError`.
    /// Compile a source that came from a JS string, undoing the WTF-16
    /// transport escape in the literals the module carries.
    fn compile_escaped_source(
        &self,
        source: &str,
        options: EvalCompileOptions,
    ) -> Result<BytecodeModule, VmError> {
        let mut module = self.compile_eval_source(source, options)?;
        crate::eval_source::decode_module(&mut module);
        Ok(module)
    }

    fn compile_eval_source(
        &self,
        source: &str,
        options: EvalCompileOptions,
    ) -> Result<BytecodeModule, VmError> {
        let hook = self.eval_hook.as_ref().ok_or_else(|| {
            self.err_syntax(
                ("eval / new Function are disabled (no compiler hook installed)".to_string())
                    .into(),
            )
        })?;
        hook(source, options).map_err(|message| self.err_syntax(message.into()))
    }
}

fn is_v8_native_eval_hint(source: &str) -> bool {
    let trimmed = source.trim();
    (trimmed.starts_with("%PrepareFunctionForOptimization(")
        || trimmed.starts_with("%OptimizeFunctionOnNextCall("))
        && trimmed.ends_with(')')
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{Function, Instruction, Op, SourceKind, SpanEntry};

    fn eval_module(source: &str) -> BytecodeModule {
        let code = match source {
            "ok" => vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(0), Operand::Imm32(42)],
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)],
                },
            ],
            "throw" => vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadInt32,
                    operands: vec![Operand::Register(0), Operand::Imm32(91)],
                },
                Instruction {
                    pc: 1,
                    op: Op::Throw,
                    operands: vec![Operand::Register(0)],
                },
            ],
            "invalid" => vec![Instruction {
                pc: 0,
                op: Op::LoadString,
                operands: vec![Operand::Register(0), Operand::ConstIndex(0)],
            }],
            other => panic!("unexpected eval test source: {other}"),
        };
        let spans = code
            .iter()
            .map(|instruction| SpanEntry {
                pc: instruction.pc,
                span: (0, 0),
            })
            .collect();
        BytecodeModule {
            module: "direct-eval-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                name: "<main>".to_string(),
                scratch: 1,
                code: code.into(),
                spans,
                ..Function::default()
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        }
    }

    fn source_value(interp: &mut Interpreter, source: &str) -> Value {
        Value::string(
            crate::string::JsString::from_str(source, interp.gc_heap_mut())
                .expect("short eval test source fits"),
        )
    }

    fn direct_options() -> EvalCompileOptions {
        EvalCompileOptions {
            caller_chain: Some(otter_bytecode::EvalCallerChain::default()),
            ..EvalCompileOptions::default()
        }
    }

    #[test]
    fn direct_eval_shared_stack_reclaims_every_nested_completion_path() {
        let mut interp = Interpreter::new();
        interp.set_eval_hook(Some(std::sync::Arc::new(|source, _| {
            Ok(eval_module(source))
        })));

        // Register 1 holds the caller's (absent) context.
        let caller_function = Function {
            id: 777,
            locals: 2,
            ..Function::default()
        };
        let mut caller = interp
            .test_frame_for_function(&caller_function)
            .expect("caller register window");
        caller.pc = 19;
        caller.registers[0] = Value::number_i32(7);
        let mut stack = crate::test_support::FrameChainFixture::new();
        stack.push(caller);

        interp.with_runtime_turn(&mut stack, |turn| {
            let (interp, stack) = turn.into_parts();

            let ok = source_value(interp, "ok");
            assert_eq!(
                interp
                    .run_direct_eval(&ok, direct_options(), 1, false, stack)
                    .expect("direct eval succeeds"),
                Value::number_i32(42)
            );
            assert_caller_only(interp, stack);

            let thrown_source = source_value(interp, "throw");
            let thrown = interp
                .run_direct_eval(&thrown_source, direct_options(), 1, false, stack)
                .expect_err("eval throw escapes its activation floor");
            assert!(matches!(thrown, VmError::Uncaught));
            assert_eq!(
                interp.take_pending_uncaught_throw(),
                Some(Value::number_i32(91))
            );
            assert_caller_only(interp, stack);

            let invalid = source_value(interp, "invalid");
            let error = interp
                .run_direct_eval(&invalid, direct_options(), 1, false, stack)
                .expect_err("invalid bytecode leaves cleanup to the eval boundary");
            assert!(matches!(error, VmError::InvalidOperand));
            assert_caller_only(interp, stack);

            let _caller = stack.pop().expect("caller retained");
        });
    }

    fn assert_caller_only(_interp: &Interpreter, stack: &ActivationStack) {
        assert_eq!(stack.len(), 1);
        let caller = stack.last().expect("caller retained");
        assert_eq!(caller.function_id, 777);
        assert_eq!(caller.pc, 19);
        assert_eq!(caller.registers[0], Value::number_i32(7));
    }
}
