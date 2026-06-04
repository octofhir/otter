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
//!
//! # See also
//! - [`crate::code_space`]
//! - [`crate::ExecutionContext`]

use otter_bytecode::{BytecodeModule, Operand};
use smallvec::SmallVec;

use crate::promise::JsPromise;
use crate::{
    AsyncFrameState, EvalCompileOptions, ExecutionContext, Frame, Interpreter, Value, VmError,
    abstract_ops, operand_decode::register_operand, promise_dispatch, read_register,
    render_thrown_value, write_register,
};

impl Interpreter {
    pub(crate) fn run_eval_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let src_reg = register_operand(operands.get(1))?;
        let forbid_var_arguments = matches!(operands.get(2), Some(&Operand::Imm32(1)));
        let top_idx = stack.len() - 1;
        let value = *read_register(&stack[top_idx], src_reg)?;
        let force_strict = context.function_is_strict(stack[top_idx].function_id);
        let result = self.run_eval(
            &value,
            EvalCompileOptions {
                force_strict,
                forbid_var_arguments,
            },
        )?;
        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_new_function_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let argc = match operands.get(1) {
            Some(&Operand::ConstIndex(n)) => n as usize,
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
            parts.push(self.function_constructor_arg_to_string(context, &value)?);
        }
        let result = self.build_function_constructor_from_parts(parts)?;
        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// Execute `eval(source)` per §19.4.1.1 indirect-eval semantics:
    /// parse + compile via the embedder hook, then run `<main>`
    /// on a sub-stack. The current dispatch loop's stack stays
    /// untouched.
    ///
    /// # Errors
    /// - [`VmError::SyntaxError`] when no eval hook is installed or
    ///   parsing / compilation fail.
    pub(crate) fn run_eval(
        &mut self,
        value: &Value,
        options: EvalCompileOptions,
    ) -> Result<Value, VmError> {
        let Some(s) = value.as_string(&self.gc_heap) else {
            // Per §19.4.1.1 step 4, eval'd non-strings are returned
            // unchanged — `eval(42) === 42`.
            return Ok(*value);
        };
        let source = s.to_lossy_string(&self.gc_heap);
        let module = self.compile_eval_source(&source, options)?;
        // Linking (not a standalone context) keeps the eval chunk's
        // function ids global, so closures and classes escaping the
        // eval stay callable from any later frame.
        let context = self.link_module(module);
        let main = context.exec_main();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, main, Frame::empty_upvalues())?;
        let entry_this = if main.is_module || main.is_strict {
            Value::undefined()
        } else {
            Value::object(self.global_this)
        };
        let entry = Frame::with_exec_return_upvalues_and_this(main, None, upvalues, entry_this);
        let entry_is_async = main.is_async;
        stack.push(entry);
        let entry_promise = if entry_is_async {
            let result = promise_dispatch::PromiseBuilder::with_context(context.clone())
                .pending_stack_rooted(self, &stack, &[], &[])?;
            stack
                .last_mut()
                .expect("entry frame was just pushed")
                .async_state = Some(AsyncFrameState {
                result_promise: result,
            });
            Some(result)
        } else {
            None
        };
        let value = self.dispatch_loop(&context, &mut stack)?;
        if let Some(promise) = entry_promise {
            // Drain microtasks attached to top-level await so the
            // entry promise settles before we read its value.
            self.drain_microtasks_with_default(Some(context))
                .map_err(|e| e.error)?;
            return Ok(match promise.state(&self.gc_heap) {
                crate::promise::PromiseState::Fulfilled(v) => v,
                crate::promise::PromiseState::Rejected(reason) => {
                    return Err(VmError::Uncaught {
                        value: render_thrown_value(&reason, &self.gc_heap),
                    });
                }
                crate::promise::PromiseState::Pending => Value::undefined(),
            });
        }
        Ok(value)
    }

    /// Build a `Function(args, body)` callable per §20.2.1.1. `args`
    /// must live in GC-visited slots (native-call argument storage or
    /// frame registers) because per-argument coercion can re-enter
    /// user code and move the heap; each iteration re-reads its slot.
    /// The synthesised module links into the interpreter's code space,
    /// so the returned closure's function id resolves from any frame —
    /// no wrapper indirection is needed.
    pub(crate) fn build_function_constructor(
        &mut self,
        context: &ExecutionContext,
        args: &[Value],
    ) -> Result<Value, VmError> {
        // Coerce every argument to a string per §20.2.1.1 step 1.
        let mut parts: Vec<String> = Vec::with_capacity(args.len());
        for arg in args {
            parts.push(self.function_constructor_arg_to_string(context, arg)?);
        }
        self.build_function_constructor_from_parts(parts)
    }

    /// §20.2.1.1 steps 2+ over already-coerced argument strings.
    pub(crate) fn build_function_constructor_from_parts(
        &mut self,
        parts: Vec<String>,
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
        let source = format!("(function anonymous({params_joined}) {{\n{body}\n}})");
        let module = self.compile_eval_source(&source, EvalCompileOptions::default())?;
        let context = self.link_module(module);
        // Running the synthesised module's `<main>` returns the
        // function value (the parenthesised expression is the
        // program's completion).
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function_with_heap(
            context.main(),
            &mut self.gc_heap,
        )?);
        self.dispatch_loop(&context, &mut stack)
    }

    fn function_constructor_arg_to_string(
        &mut self,
        context: &ExecutionContext,
        value: &Value,
    ) -> Result<String, VmError> {
        let primitive = if value.is_object() || value.is_proxy() {
            self.to_primitive_string_hint_sync(context, *value)?
        } else {
            *value
        };
        if let Some(s) = primitive.as_string(&self.gc_heap) {
            return Ok(s.to_lossy_string(&self.gc_heap));
        }
        if primitive.is_symbol() {
            return Err(VmError::TypeError {
                message: "Cannot convert a Symbol value to a string".to_string(),
            });
        }
        Ok(primitive.display_string(&self.gc_heap))
    }

    // `to_*` mirrors the spec abstract operation `ToPrimitive` (§7.1.1).
    // The interpreter borrow is `&mut self` because the helper invokes
    // user-defined `toString` / `valueOf`, which can re-enter dispatch.
    #[allow(clippy::wrong_self_convention)]
    fn to_primitive_string_hint_sync(
        &mut self,
        context: &ExecutionContext,
        value: Value,
    ) -> Result<Value, VmError> {
        for method in ["toString", "valueOf"] {
            let callee = self.get_property_value_for_call(context, value, method)?;
            if !self.is_callable_runtime(&callee) {
                continue;
            }
            let result = self.run_callable_sync(context, &callee, value, SmallVec::new())?;
            if abstract_ops::is_primitive(&result) {
                return Ok(result);
            }
        }
        Err(VmError::TypeError {
            message: "Cannot convert object to primitive value".to_string(),
        })
    }

    /// Helper — invoke the eval hook, mapping its error to a
    /// VmError that the throwable-conversion path will surface as
    /// `SyntaxError`.
    fn compile_eval_source(
        &self,
        source: &str,
        options: EvalCompileOptions,
    ) -> Result<BytecodeModule, VmError> {
        let hook = self
            .eval_hook
            .as_ref()
            .ok_or_else(|| VmError::SyntaxError {
                message: "eval / new Function are disabled (no compiler hook installed)"
                    .to_string(),
            })?;
        hook(source, options).map_err(|message| VmError::SyntaxError { message })
    }
}
