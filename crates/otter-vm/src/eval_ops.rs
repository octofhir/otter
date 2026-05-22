//! Eval and dynamic function constructor opcode helpers.
//!
//! `eval` and `new Function(...)` recurse through the VM compiler/runtime path,
//! so their dispatch has to run before the dense in-frame match borrows the
//! current frame.
//!
//! # Contents
//! - Indirect eval execution and writeback.
//! - `Function` constructor argument collection.
//!
//! # Invariants
//! - Helpers advance the current frame PC exactly once on success.
//! - Arguments are read from executable operands.
//! - Strict-mode eval inherits the caller function strictness.
//!
//! # See also
//! - [`crate::executable`]
//! - [`crate::ExecutionContext`]

use otter_bytecode::{BytecodeModule, Operand};
use otter_gc::raw::RawGc;
use smallvec::SmallVec;

use crate::promise::JsPromise;
use crate::{
    AsyncFrameState, EvalCompileOptions, ExecutionContext, Frame, Interpreter, NativeCtx, Value,
    VmError, abstract_ops, function_metadata, native_function, object,
    operand_decode::register_operand, promise_dispatch, read_register, render_thrown_value,
    write_register,
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
        let top_idx = stack.len() - 1;
        let value = *read_register(&stack[top_idx], src_reg)?;
        let force_strict = context.function_is_strict(stack[top_idx].function_id);
        let result = self.run_eval(&value, force_strict)?;
        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)?;
        frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        Ok(())
    }

    pub(crate) fn run_new_function_operands(
        &mut self,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let top_idx = stack.len() - 1;
        let args = collect_new_function_args(&stack[top_idx], operands)?;
        let result = self.build_function_constructor_with_roots(
            context,
            &args,
            Some(stack),
            &[],
            &[args.as_slice()],
        )?;
        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)?;
        frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
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
    pub(crate) fn run_eval(&mut self, value: &Value, force_strict: bool) -> Result<Value, VmError> {
        let source = match value {
            Value::String(s) => s.to_lossy_string(&self.gc_heap),
            // Per §19.4.1.1 step 4, eval'd non-strings are returned
            // unchanged — `eval(42) === 42`.
            _ => return Ok(*value),
        };
        let module = self.compile_eval_source(&source, EvalCompileOptions { force_strict })?;
        let context = ExecutionContext::from_module(module);
        let main = context.exec_main();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, main, Frame::empty_upvalues())?;
        let entry_this = if main.is_module || main.is_strict {
            Value::Undefined
        } else {
            Value::Object(self.global_this)
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

    /// Build a `Function(args, body)` callable per §20.2.1.1. The
    /// result is a [`crate::NativeFunction`] that holds the freshly
    /// compiled inner module and dispatches it on every call;
    /// inner-module function IDs aren't valid against the outer
    /// running module, so wrapping in a native rather than
    /// returning the inner closure handle directly keeps the call
    /// surface correct.
    pub(crate) fn build_function_constructor_with_roots(
        &mut self,
        context: &ExecutionContext,
        args: &[Value],
        stack_roots: Option<&SmallVec<[Frame; 8]>>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        // Coerce every argument to a string per §20.2.1.1 step 1.
        let mut parts: Vec<String> = Vec::with_capacity(args.len());
        for arg in args {
            parts.push(self.function_constructor_arg_to_string(context, arg)?);
        }
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
        let context = ExecutionContext::from_module(module);
        // Running the synthesised module's `<main>` returns the
        // function value (the parenthesised expression is the
        // program's completion). We capture that value's
        // `function_id` together with the inner context so the
        // returned native can replay calls against the right
        // bytecode.
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function_with_heap(
            context.main(),
            &mut self.gc_heap,
        )?);
        let value = self.dispatch_loop(&context, &mut stack)?;
        self.wrap_eval_function_value_with_roots(
            context,
            value,
            stack_roots,
            value_roots,
            slice_roots,
        )
    }

    fn wrap_eval_function_value_with_roots(
        &mut self,
        function_context: ExecutionContext,
        value: Value,
        stack_roots: Option<&SmallVec<[Frame; 8]>>,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if !matches!(value, Value::Function { .. } | Value::Closure(_)) {
            return Ok(value);
        }
        let mut metadata_ctx = function_metadata::FunctionMetadataContext::new(
            &function_context,
            &mut self.gc_heap,
            &self.function_user_props,
            &self.function_deleted_metadata,
        );
        let name_value =
            function_metadata::callable_intrinsic_property(&mut metadata_ctx, &value, "name")?;
        let length_value =
            function_metadata::callable_intrinsic_property(&mut metadata_ctx, &value, "length")?;
        let prototype_value = match &value {
            Value::Function { function_id }
            | Value::Closure(crate::closure::JsClosure {
                cached_function_id: function_id,
                ..
            }) => {
                let mut roots = Vec::with_capacity(value_roots.len() + 1);
                roots.push(&value);
                roots.extend_from_slice(value_roots);
                match stack_roots {
                    Some(stack) => self.function_property_get_stack_rooted(
                        &function_context,
                        stack,
                        *function_id,
                        "prototype",
                    )?,
                    None => self.function_property_get_runtime_rooted(
                        &function_context,
                        *function_id,
                        "prototype",
                        &roots,
                        slice_roots,
                    )?,
                }
            }
            _ => Value::undefined(),
        };
        let target_capture = value;
        let callback_context = function_context.clone();
        let stack_slots = stack_roots
            .map(|stack| self.collect_allocation_roots(stack))
            .unwrap_or_default();
        let native_value_root = value;
        let name_root = name_value;
        let length_root = length_value;
        let prototype_root = prototype_value;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &stack_slots {
                visitor(slot);
            }
            native_value_root.trace_value_slots(visitor);
            name_root.trace_value_slots(visitor);
            length_root.trace_value_slots(visitor);
            prototype_root.trace_value_slots(visitor);
            for root in value_roots {
                root.trace_value_slots(visitor);
            }
            for slice in slice_roots {
                for value in *slice {
                    value.trace_value_slots(visitor);
                }
            }
        };
        let wrapper = native_function::native_constructor_value_with_captures_unchecked_with_roots(
            &mut self.gc_heap,
            "anonymous",
            smallvec::smallvec![target_capture],
            &mut external_visit,
            move |ctx: &mut NativeCtx<'_>, call_args: &[Value], captures: &[Value]| {
                let Some(target) = captures.first().cloned() else {
                    return Err(crate::native_function::NativeError::TypeError {
                        name: "anonymous",
                        reason: "missing wrapped function target".to_string(),
                    });
                };
                let args: SmallVec<[Value; 8]> = call_args.iter().cloned().collect();
                let is_construct_call = ctx.is_construct_call();
                let this_value = *ctx.this_value();
                let interp = ctx.interp_mut();
                let result = if is_construct_call {
                    interp.run_construct_sync(&callback_context, &target, target, args)
                } else {
                    interp.run_callable_sync(&callback_context, &target, this_value, args)
                }
                .map_err(|err| crate::native_function::NativeError::TypeError {
                    name: "anonymous",
                    reason: format!("{err}"),
                })?;
                interp
                    .wrap_eval_function_value_with_roots(
                        callback_context.clone(),
                        result,
                        None,
                        &[&target, &this_value],
                        &[call_args],
                    )
                    .map_err(|err| crate::native_function::NativeError::TypeError {
                        name: "anonymous",
                        reason: format!("{err}"),
                    })
            },
        )
        .map_err(VmError::from)?;

        if let Value::NativeFunction(native) = &wrapper {
            let name = object::PropertyDescriptor::data(name_value, false, false, true);
            let _ = native.define_own_property(&mut self.gc_heap, "name", name);
            let length = object::PropertyDescriptor::data(length_value, false, false, true);
            let _ = native.define_own_property(&mut self.gc_heap, "length", length);
            let prototype = object::PropertyDescriptor::data(prototype_value, true, false, false);
            let _ = native.define_own_property(&mut self.gc_heap, "prototype", prototype);
            if let Value::Object(proto) = prototype_value {
                let constructor = object::PropertyDescriptor::data(wrapper, true, false, true);
                let _ = object::define_own_property(
                    proto,
                    &mut self.gc_heap,
                    "constructor",
                    constructor,
                );
            }
        }

        Ok(wrapper)
    }

    fn function_constructor_arg_to_string(
        &mut self,
        context: &ExecutionContext,
        value: &Value,
    ) -> Result<String, VmError> {
        let primitive = match value {
            Value::Object(_) | Value::Proxy(_) => {
                self.to_primitive_string_hint_sync(context, *value)?
            }
            other => *other,
        };
        match primitive {
            Value::String(s) => Ok(s.to_lossy_string(&self.gc_heap)),
            Value::Symbol(_) => Err(VmError::TypeError {
                message: "Cannot convert a Symbol value to a string".to_string(),
            }),
            other => Ok(other.display_string(&self.gc_heap)),
        }
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

fn collect_new_function_args(
    frame: &Frame,
    operands: &[Operand],
) -> Result<SmallVec<[Value; 4]>, VmError> {
    let argc = match operands.get(1) {
        Some(&Operand::ConstIndex(n)) => n as usize,
        _ => return Err(VmError::InvalidOperand),
    };
    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
    for i in 0..argc {
        let r = register_operand(operands.get(2 + i))?;
        args.push(*read_register(frame, r)?);
    }
    Ok(args)
}
