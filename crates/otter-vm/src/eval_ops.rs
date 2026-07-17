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
//!
//! # See also
//! - [`crate::code_space`]
//! - [`crate::ExecutionContext`]

use crate::activation_stack::ActivationStack;
use otter_bytecode::{BytecodeModule, Operand};
use smallvec::SmallVec;

use crate::promise::JsPromise;
use crate::{
    AsyncFrameState, EvalCallerBinding, EvalCompileOptions, ExecutionContext, Frame, Interpreter,
    Value, VmError, abstract_ops, operand_decode::register_operand, promise_dispatch,
    read_register, write_register,
};

/// Where one caller-scope cell for a direct eval comes from. Cells
/// are re-read from the caller frame *after* every GC-allocating
/// step (compile, link, spine building) because young-generation
/// collections move upvalue cells; only the frame's traced slots
/// stay current.
enum CallerCellSource {
    /// Slot in the caller frame's upvalue array (compile-time
    /// promoted function-scope binding).
    Upvalue(u16),
    /// Entry in the caller frame's runtime eval-introduced binding
    /// map (created by an earlier direct eval from the same frame).
    EvalVar(String),
}

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
    pub(crate) fn run_eval_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        operands: impl crate::executable::OperandSource,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let src_reg = register_operand(operands.get(1))?;
        let flags = match operands.get(2) {
            Some(Operand::Imm32(bits)) => bits,
            _ => 0,
        };
        let forbid_var_arguments = flags & 1 != 0;
        let in_param_init = flags & 2 != 0;
        let new_target_allowed = flags & 4 != 0;
        let new_target_suppressed = flags & 8 != 0;
        let super_property_allowed = flags & 16 != 0;
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
        let force_strict = context.function_is_strict(stack[top_idx].function_id);
        // §19.2.1.3 EvalDeclarationInstantiation — a direct eval
        // inside a function receives the caller variable environment.
        // The compiler promoted every caller function-scope binding
        // into a cell and recorded the name → slot table; earlier
        // evals may have extended the frame with more named cells.
        let (caller_scope, cell_sources) =
            self.collect_caller_scope(context, &stack[top_idx], in_param_init);
        // §19.2.1.1 `inFunction` — the compiler's flag, not table
        // emptiness: a synthesized constructor may carry no bindings
        // yet still host a field-initializer eval.
        let in_function_caller = context
            .exec_function(stack[top_idx].function_id)
            .is_some_and(|function| function.contains_direct_eval);
        let result = if !in_function_caller && cell_sources.is_empty() {
            // Script-top-level direct eval: the caller variable
            // environment *is* the global environment, which the
            // compiled chunk reaches through the global mirror.
            self.run_eval(
                stack,
                &value,
                EvalCompileOptions {
                    force_strict,
                    forbid_var_arguments,
                    caller_scope: None,
                    script_goal: false,
                    new_target_allowed,
                    in_class_field_initializer: new_target_suppressed,
                    super_property_allowed,
                },
            )?
        } else {
            self.run_direct_eval(
                &value,
                EvalCompileOptions {
                    force_strict,
                    forbid_var_arguments,
                    caller_scope: Some(caller_scope),
                    script_goal: false,
                    new_target_allowed,
                    in_class_field_initializer: new_target_suppressed,
                    super_property_allowed,
                },
                &cell_sources,
                new_target_suppressed,
                stack,
            )?
        };
        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// Build the caller-scope binding list (compiler-facing names)
    /// and the matching cell-source list (runtime cell origins) for
    /// a direct eval running on `frame`. Entry `i` of both lists
    /// describes upvalue slot `i` of the compiled eval `<main>`.
    fn collect_caller_scope(
        &self,
        context: &ExecutionContext,
        frame: &Frame,
        in_param_init: bool,
    ) -> (Vec<EvalCallerBinding>, Vec<CallerCellSource>) {
        let mut scope: Vec<EvalCallerBinding> = Vec::new();
        let mut sources: Vec<CallerCellSource> = Vec::new();
        if let Some(function) = context.exec_function(frame.function_id) {
            for binding in function.direct_eval_bindings.iter() {
                // §10.2.11 — body lexical bindings don't exist yet
                // while parameter initializers run; an eval there
                // neither sees them nor collides with them.
                if in_param_init && binding.lexical {
                    continue;
                }
                scope.push(EvalCallerBinding {
                    name: binding.name.to_string(),
                    lexical: binding.lexical,
                    captured: binding.captured,
                    is_const: binding.is_const,
                    fn_self_name: binding.fn_self_name,
                });
                sources.push(CallerCellSource::Upvalue(binding.upvalue));
            }
        }
        if let Some(eval_vars) = self
            .frame_cold(frame)
            .and_then(|cold| cold.eval_vars.as_deref())
        {
            // Deterministic order for the compiled chunk's slot
            // layout — the map itself is hash-ordered.
            let mut names: Vec<&String> = eval_vars.keys().collect();
            names.sort();
            for name in names {
                scope.push(EvalCallerBinding {
                    name: name.clone(),
                    lexical: false,
                    captured: false,
                    is_const: false,
                    fn_self_name: false,
                });
                sources.push(CallerCellSource::EvalVar(name.clone()));
            }
        }
        (scope, sources)
    }

    /// Execute a direct eval whose caller variable environment is a
    /// function environment (§19.2.1.1 PerformEval with
    /// `direct = true`). The compiled chunk's leading upvalue slots
    /// alias the caller's binding cells; new var-scoped bindings the
    /// body introduces are adopted into the caller frame's
    /// eval-binding map before the body runs (hoisting), and `this`
    /// is inherited from the caller.
    fn run_direct_eval(
        &mut self,
        value: &Value,
        options: EvalCompileOptions,
        cell_sources: &[CallerCellSource],
        new_target_suppressed: bool,
        stack: &mut ActivationStack,
    ) -> Result<Value, VmError> {
        let Some(s) = value.as_string(&self.gc_heap) else {
            // §19.2.1.1 step 2 — non-string operands are returned
            // unchanged.
            return Ok(*value);
        };
        let source = s.to_lossy_string(&self.gc_heap);
        // §19.2.1.3 — only the caller's OWN variable-environment
        // names block adoption; a passthrough CAPTURE of the same
        // name still receives a fresh caller binding.
        let caller_scope_names: std::collections::HashSet<String> = options
            .caller_scope
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter(|binding| !binding.captured)
            .map(|binding| binding.name.clone())
            .collect();
        let module = self.compile_eval_source(&source, options)?;
        let context = self.link_module(module);
        let main = context.exec_main();
        let mut upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, main, Frame::empty_upvalues())?;
        // Splice the caller's cells into the reserved leading slots
        // and adopt the chunk's new var-binding cells into the caller
        // frame. No GC allocation happens from here until the spine
        // is rooted on the entry frame — the cells read below are
        // only current while nothing moves the heap.
        let top_idx = stack.len() - 1;
        {
            let caller = &stack[top_idx];
            let cold_eval_vars = self
                .frame_cold(caller)
                .and_then(|cold| cold.eval_vars.as_deref());
            for (i, cell_source) in cell_sources.iter().enumerate() {
                let cell = match cell_source {
                    CallerCellSource::Upvalue(idx) => caller
                        .upvalues
                        .get(*idx as usize)
                        .copied()
                        .ok_or(VmError::InvalidOperand)?,
                    CallerCellSource::EvalVar(name) => cold_eval_vars
                        .and_then(|map| map.get(name))
                        .copied()
                        .ok_or(VmError::InvalidOperand)?,
                };
                *upvalues.get_mut(i).ok_or(VmError::InvalidOperand)? = cell;
            }
        }
        // §19.2.1.3 step 16.b — the body's *new* var-scoped bindings
        // become caller-environment bindings before the body runs,
        // matching var hoisting semantics. The chunk's table also
        // lists caller-scope re-binds and its own lexicals (for
        // nested evals); both are excluded from adoption.
        let adopted: Vec<(String, crate::UpvalueCell)> = main
            .direct_eval_bindings
            .iter()
            .filter(|binding| {
                !binding.lexical && !caller_scope_names.contains(binding.name.as_ref())
            })
            .filter_map(|binding| {
                upvalues
                    .get(binding.upvalue as usize)
                    .map(|cell| (binding.name.to_string(), *cell))
            })
            .collect();
        let entry_this = stack[top_idx].this_value;
        // §13.3.3 — `new.target` in the eval body reads the caller
        // frame's value (direct eval is contained in function code).
        // Class field initializers observe `undefined` (§15.7.10).
        let caller_new_target = if new_target_suppressed {
            None
        } else {
            self.frame_cold(&stack[top_idx])
                .and_then(|cold| cold.new_target)
        };
        if !adopted.is_empty() {
            // §9.1 — adopted bindings land in BOTH stores: the
            // legacy per-frame map (same-frame dynamic reads) and
            // the GC-owned eval environment record that closures
            // created in this frame capture (cross-closure and
            // outlives-the-frame visibility).
            let env = self
                .frame_cold(&stack[top_idx])
                .and_then(|cold| cold.eval_env);
            let env = match env {
                Some(env) => Some(env),
                None => {
                    let fresh = crate::eval_env::alloc_eval_env(&mut self.gc_heap, None)
                        .map_err(crate::oom_to_vm)?;
                    self.frame_ensure_cold(&mut stack[top_idx]).eval_env = Some(fresh);
                    Some(fresh)
                }
            };
            for (name, cell) in adopted {
                {
                    let cold = self.frame_ensure_cold(&mut stack[top_idx]);
                    let map = cold.eval_vars.get_or_insert_default();
                    map.insert(name.clone(), cell);
                }
                if let Some(env) = env {
                    crate::eval_env::eval_env_insert(&mut self.gc_heap, env, name, cell);
                }
            }
        }
        let main = context.exec_main();
        let window = self.alloc_reg_window(main.register_count as usize)?;
        let mut entry =
            Frame::with_exec_return_upvalues_and_this(main, None, upvalues, entry_this, window);
        if caller_new_target.is_some() {
            self.frame_ensure_cold(&mut entry).new_target = caller_new_target;
        }
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
        let source = s.to_lossy_string(&self.gc_heap);
        if is_v8_native_eval_hint(&source) {
            return Ok(Value::undefined());
        }
        let module = self.compile_eval_source(&source, options)?;
        // Linking (not a standalone context) keeps the eval chunk's
        // function ids global, so closures and classes escaping the
        // eval stay callable from any later frame.
        let context = self.link_module(module);
        let main = context.exec_main();
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, main, Frame::empty_upvalues())?;
        // §19.2.1.3 — eval code evaluated at global scope (direct at
        // the top level or indirect) binds `this` to globalThis even
        // when the eval source itself is strict; only module code gets
        // an undefined top-level `this`.
        let entry_this = if main.is_module {
            Value::undefined()
        } else {
            Value::object(self.global_this)
        };
        let window = self.alloc_reg_window(main.register_count as usize)?;
        let entry =
            Frame::with_exec_return_upvalues_and_this(main, None, upvalues, entry_this, window);
        let entry_is_async = main.is_async;
        let floor = stack.floor();
        stack.push(entry);
        self.with_handle_scope(|interp, scope| {
            let entry_promise = if entry_is_async {
                let result = promise_dispatch::PromiseBuilder::with_context(context.clone())
                    .pending_stack_rooted(interp, stack, &[], &[])?;
                let frame = stack.last_mut().expect("entry frame was just pushed");
                interp.frame_set_async_state(
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
        // `(line, column)` against it.
        self.register_module_source(
            module_url.to_string(),
            std::sync::Arc::from(source.as_str()),
        );
        let context = self.link_module(module);
        // Running the synthesised module's `<main>` returns the wrapper
        // function value (the parenthesised expression is the program's
        // completion).
        let main = context.main();
        let total = main
            .param_count
            .saturating_add(main.locals)
            .saturating_add(main.scratch) as usize;
        let window = self.alloc_reg_window(total)?;
        let floor = stack.floor();
        stack.push(Frame::for_function_with_heap(
            main,
            &mut self.gc_heap,
            window,
        )?);
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
        let source = format!("({prefix} anonymous({params_joined}\n) {{\n{body}\n}})");
        let module = self.compile_eval_source(&source, EvalCompileOptions::default())?;
        let context = self.link_module(module);
        // Running the synthesised module's `<main>` returns the
        // function value (the parenthesised expression is the
        // program's completion).
        let main = context.main();
        let total = main
            .param_count
            .saturating_add(main.locals)
            .saturating_add(main.scratch) as usize;
        let window = self.alloc_reg_window(total)?;
        let floor = stack.floor();
        stack.push(Frame::for_function_with_heap(
            main,
            &mut self.gc_heap,
            window,
        )?);
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
            return Ok(s.to_lossy_string(&self.gc_heap));
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
            caller_scope: Some(Vec::new()),
            ..EvalCompileOptions::default()
        }
    }

    #[test]
    fn direct_eval_shared_stack_reclaims_every_nested_completion_path() {
        let mut interp = Interpreter::new();
        interp.set_eval_hook(Some(std::sync::Arc::new(|source, _| {
            Ok(eval_module(source))
        })));

        let caller_function = Function {
            id: 777,
            locals: 1,
            ..Function::default()
        };
        let mut caller = interp
            .test_frame_for_function(&caller_function)
            .expect("caller register window");
        caller.pc = 19;
        caller.registers[0] = Value::number_i32(7);
        let mut stack = ActivationStack::new();
        stack.push(caller);

        interp.with_runtime_turn(&mut stack, |turn| {
            let (interp, stack) = turn.into_parts();

            let ok = source_value(interp, "ok");
            assert_eq!(
                interp
                    .run_direct_eval(&ok, direct_options(), &[], false, stack)
                    .expect("direct eval succeeds"),
                Value::number_i32(42)
            );
            assert_caller_only(interp, stack);

            let thrown_source = source_value(interp, "throw");
            let thrown = interp
                .run_direct_eval(&thrown_source, direct_options(), &[], false, stack)
                .expect_err("eval throw escapes its activation floor");
            assert!(matches!(thrown, VmError::Uncaught));
            assert_eq!(
                interp.take_pending_uncaught_throw(),
                Some(Value::number_i32(91))
            );
            assert_caller_only(interp, stack);

            let invalid = source_value(interp, "invalid");
            let error = interp
                .run_direct_eval(&invalid, direct_options(), &[], false, stack)
                .expect_err("invalid bytecode leaves cleanup to the eval boundary");
            assert!(matches!(error, VmError::InvalidOperand));
            assert_caller_only(interp, stack);

            let mut caller = stack.pop().expect("caller retained");
            interp.reclaim_registers(&mut caller);
            assert_eq!(interp.register_stack.checkpoint(), 0);
        });
    }

    fn assert_caller_only(interp: &Interpreter, stack: &ActivationStack) {
        assert_eq!(stack.len(), 1);
        let caller = stack.last().expect("caller retained");
        assert_eq!(caller.function_id, 777);
        assert_eq!(caller.pc, 19);
        assert_eq!(caller.registers[0], Value::number_i32(7));
        assert_eq!(interp.register_stack.checkpoint(), 1);
    }
}
