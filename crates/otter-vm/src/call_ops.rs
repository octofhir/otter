//! Call and construct opcode helpers.
//!
//! Stack-modifying call bytecodes decode variadic executable operands, prepare
//! frames, and may immediately invoke native/proxy/constructor paths. Keeping
//! that machinery here lets `lib.rs` stay closer to a dispatch map.
//!
//! # Contents
//! - Ordinary call entry and shared callable invocation.
//! - Constructor call entry and receiver/prototype setup.
//! - Spread and explicit-`this` call forms.
//!
//! # Invariants
//! - Call-site helpers advance the caller PC before pushing or synchronously
//!   invoking another frame.
//! - `invoke` remains the shared call path for bytecode, closures, native
//!   callables, bound functions, class constructors, and proxies.
//! - Constructor dispatch preserves `new.target` and receiver substitution
//!   invariants used by `pop_frame`.
//!
//! # See also
//! - [`crate::Frame`]
//! - [`crate::executable`]

use otter_bytecode::Operand;
use otter_gc::raw::RawGc;
use smallvec::SmallVec;

use crate::{
    AsyncFrameState, ExecutableFunction, ExecutionContext, Frame, Interpreter, JsObject,
    NativeCallInfo, NativeCtx, NativeFunction, Value, VmError, VmGetOutcome, VmPropertyKey,
    abstract_ops, argument_window::BytecodeArgumentWindow, frame_state::UpvalueSpine,
    is_constructor_runtime, native_to_vm_error, operand_decode::register_operand, promise_dispatch,
    read_register, write_register,
};

struct SyncNativeCallRoots<'a> {
    /// Inner registration over the interpreter, re-dispatched so its
    /// runtime roots are re-enumerated **live** at every trace. A
    /// snapshot `Vec<*mut RawGc>` taken at call entry would miss
    /// anything the native registers afterwards (fresh anchors, IC
    /// entries, shape transitions), and a moving scavenge inside the
    /// native would then sweep or relocate objects those late slots
    /// still reference.
    interp_roots: otter_gc::ExtraRoots,
    value_roots: SmallVec<[&'a Value; 4]>,
    slice_roots: SmallVec<[&'a [Value]; 2]>,
}

impl otter_gc::ExtraRootSource for SyncNativeCallRoots<'_> {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.interp_roots.visit(visitor);
        for value in &self.value_roots {
            value.trace_value_slots(visitor);
        }
        for slice in &self.slice_roots {
            for value in *slice {
                value.trace_value_slots(visitor);
            }
        }
    }
}

fn invoke_native_call_with_roots(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    call: crate::native_function::NativeCallTarget,
    this_value: Value,
    value_roots: &[&Value],
    args: &[Value],
) -> Result<Value, VmError> {
    let this_root = this_value;
    let mut roots = SyncNativeCallRoots {
        interp_roots: otter_gc::ExtraRoots::new::<Interpreter>(interp),
        value_roots: smallvec::smallvec![&this_root],
        slice_roots: smallvec::smallvec![args],
    };
    roots.value_roots.extend_from_slice(value_roots);
    // Pushed (not installed) so any outer scope's value/slice roots
    // stay visible to scavenges triggered inside this native.
    let depth = interp
        .gc_heap
        .push_extra_roots(otter_gc::ExtraRoots::new(&roots));
    let call_info = NativeCallInfo::call(this_root);
    let mut ctx =
        NativeCtx::new_with_call_info_and_context(interp, call_info, Some(context.clone()));
    let result = call.invoke(&mut ctx, args).map_err(native_to_vm_error);
    interp.gc_heap.pop_extra_roots_to(depth - 1);
    result
}

struct PreparedBytecodeFrame {
    frame: Frame,
    is_generator: bool,
    is_async_generator: bool,
    /// Callee function id — needed to resolve the generator
    /// `[[Prototype]]` AFTER the prologue runs (§27.5.1 step 3:
    /// FunctionDeclarationInstantiation precedes
    /// OrdinaryCreateFromConstructor, so parameter side effects on
    /// `fn.prototype` are observable).
    generator_function_id: u32,
}

/// `(function_id, upvalues, this, new_target, derived_this_cell,
/// eval_env)` resolved from a callable value for a bytecode call.
pub(crate) type BytecodeCallTargetParts = (
    u32,
    crate::frame_state::UpvalueSpine,
    Value,
    Option<Value>,
    Option<crate::UpvalueCell>,
    Option<crate::eval_env::EvalEnvHandle>,
);

impl Interpreter {
    /// §9.1 — install the frame's direct-eval variable environment:
    /// a `contains_direct_eval` function gets a FRESH record chained
    /// to the closure's captured one (so probe closures created
    /// before the eval observe later bindings); other closures just
    /// re-expose the captured record for the dynamic walkers.
    pub(crate) fn stash_frame_eval_env(
        &mut self,
        function: &crate::executable::ExecutableFunction,
        frame: &mut Frame,
        inherited: Option<crate::eval_env::EvalEnvHandle>,
    ) -> Result<(), VmError> {
        if function.contains_direct_eval {
            let env = crate::eval_env::alloc_eval_env(&mut self.gc_heap, inherited)
                .map_err(crate::oom_to_vm)?;
            self.frame_ensure_cold(frame).eval_env = Some(env);
        } else if inherited.is_some() {
            self.frame_ensure_cold(frame).eval_env = inherited;
        }
        Ok(())
    }

    pub(crate) fn bind_bytecode_call_arguments(
        &mut self,
        function: &ExecutableFunction,
        frame: &mut Frame,
        args: SmallVec<[Value; 8]>,
    ) -> Result<(), VmError> {
        let bind_count = (function.param_count as usize).min(args.len());
        let total_args = args.len();
        let incoming: Option<SmallVec<[Value; 4]>> = if function.needs_arguments {
            Some(args.iter().cloned().collect())
        } else {
            None
        };
        let mut iter = args.into_iter();
        for i in 0..bind_count {
            let value = iter.next().expect("bind_count <= len");
            let slot = frame.registers.get_mut(i).ok_or(VmError::InvalidOperand)?;
            *slot = value;
        }
        let rest: Option<SmallVec<[Value; 4]>> =
            if function.has_rest && total_args > function.param_count as usize {
                Some(iter.collect())
            } else {
                None
            };
        if incoming.is_some() || rest.is_some() {
            let cold = self.frame_ensure_cold(frame);
            if let Some(v) = incoming {
                cold.incoming_args = v;
            }
            if let Some(v) = rest {
                cold.rest_args = v;
            }
        }
        Ok(())
    }

    pub(crate) fn bytecode_call_target_parts(
        current: Value,
        effective_this: Value,
        heap: &otter_gc::GcHeap,
    ) -> Result<BytecodeCallTargetParts, VmError> {
        if let Some(function_id) = current.as_function() {
            return Ok((
                function_id,
                Frame::empty_upvalues(),
                effective_this,
                None,
                None,
                None,
            ));
        }
        if let Some(c) = current.as_closure(heap) {
            let function_id = c.function_id();
            let (upvalues, bound_this, bound_new_target, bound_derived_this, eval_env) = heap
                .read_payload(c.handle, |body| {
                    let ups: crate::frame_state::UpvalueSpine =
                        body.upvalues.clone().into_boxed_slice();
                    (
                        ups,
                        body.bound_this,
                        body.bound_new_target,
                        body.bound_derived_this,
                        body.eval_env,
                    )
                });
            let this_value = bound_this.unwrap_or(effective_this);
            return Ok((
                function_id,
                upvalues,
                this_value,
                bound_new_target,
                bound_derived_this,
                eval_env,
            ));
        }
        Err(VmError::NotCallable)
    }

    fn bytecode_construct_target_parts(
        current: Value,
        heap: &otter_gc::GcHeap,
    ) -> Result<(u32, crate::frame_state::UpvalueSpine), VmError> {
        if let Some(function_id) = current.as_function() {
            return Ok((function_id, Frame::empty_upvalues()));
        }
        if let Some(c) = current.as_closure(heap) {
            let function_id = c.function_id();
            let upvalues =
                heap.read_payload(c.handle, |body| body.upvalues.clone().into_boxed_slice());
            return Ok((function_id, upvalues));
        }
        Err(VmError::NotCallable)
    }

    fn build_construct_bytecode_frame(
        &mut self,
        context: &ExecutionContext,
        current: Value,
        receiver: JsObject,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
        return_register: Option<u16>,
    ) -> Result<Frame, VmError> {
        let (function_id, parent_upvalues) =
            Self::bytecode_construct_target_parts(current, &self.gc_heap)?;
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        // §10.2.5 — only ordinary functions get a [[Construct]] slot;
        // async functions, generators, async generators, and
        // MethodDefinition bodies are not constructors.
        if function.is_async || function.is_generator || function.is_method {
            return Err(VmError::TypeError {
                message: "function is not a constructor".to_string(),
            });
        }
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        // §10.2.2 — a derived constructor enters with `this` in the
        // TDZ; `super(...)` binds it via `Op::BindThisValue`. A base
        // constructor receives the freshly-allocated receiver as
        // `this` immediately.
        let is_derived = function.is_derived_constructor;
        let this_value = if is_derived {
            Value::hole()
        } else {
            Value::object(receiver)
        };
        let mut frame = Frame::with_exec_return_upvalues_and_this(
            function,
            return_register,
            upvalues,
            this_value,
        );
        let callee_closure = current.as_closure(&self.gc_heap);
        let derived_this_cell = if is_derived {
            Some(crate::alloc_upvalue(&mut self.gc_heap, Value::hole())?)
        } else {
            None
        };
        {
            let cold = self.frame_ensure_cold(&mut frame);
            if is_derived {
                cold.is_derived_constructor = true;
                cold.derived_this_cell = derived_this_cell;
            } else {
                cold.construct_target = Some(receiver);
            }
            cold.new_target = Some(new_target);
            cold.callee_closure = callee_closure;
        }
        self.bind_bytecode_call_arguments(function, &mut frame, args)?;
        Ok(frame)
    }

    fn build_construct_bytecode_frame_from_window(
        &mut self,
        context: &ExecutionContext,
        current: Value,
        receiver: JsObject,
        new_target: Value,
        args: &BytecodeArgumentWindow<'_>,
        return_register: Option<u16>,
    ) -> Result<Frame, VmError> {
        let (function_id, parent_upvalues) =
            Self::bytecode_construct_target_parts(current, &self.gc_heap)?;
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        // §10.2.5 — async / generator functions and methods are not
        // constructors.
        if function.is_async || function.is_generator || function.is_method {
            return Err(VmError::TypeError {
                message: "function is not a constructor".to_string(),
            });
        }
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        let is_derived = function.is_derived_constructor;
        let this_value = if is_derived {
            Value::hole()
        } else {
            Value::object(receiver)
        };
        let mut frame = Frame::with_exec_return_upvalues_and_this(
            function,
            return_register,
            upvalues,
            this_value,
        );
        let callee_closure = current.as_closure(&self.gc_heap);
        let extras = args.bind_into(function, &mut frame)?;
        let derived_this_cell = if is_derived {
            Some(crate::alloc_upvalue(&mut self.gc_heap, Value::hole())?)
        } else {
            None
        };
        {
            let cold = self.frame_ensure_cold(&mut frame);
            if is_derived {
                cold.is_derived_constructor = true;
                cold.derived_this_cell = derived_this_cell;
            } else {
                cold.construct_target = Some(receiver);
            }
            cold.new_target = Some(new_target);
            cold.callee_closure = callee_closure;
            if !extras.is_empty() {
                cold.rest_args = extras.rest_args;
                cold.incoming_args = extras.incoming_args;
            }
        }
        Ok(frame)
    }

    fn invoke_native_construct(
        &mut self,
        context: &ExecutionContext,
        native: NativeFunction,
        this_value: &Value,
        new_target: &Value,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let call = native.call_target(&self.gc_heap);
        let call_info = NativeCallInfo::construct(*this_value, Some(*new_target));
        self.record_runtime_native_call();
        let mut ctx =
            NativeCtx::new_with_call_info_and_context(self, call_info, Some(context.clone()));
        let result = call.invoke(&mut ctx, args).map_err(native_to_vm_error)?;
        Ok(if result.is_object_type() {
            result
        } else {
            *this_value
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn push_bytecode_call_frame(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        function_id: u32,
        parent_upvalues: UpvalueSpine,
        this_for_callee: Value,
        new_target_for_callee: Option<Value>,
        derived_this_cell: Option<crate::UpvalueCell>,
        callee_eval_env: Option<crate::eval_env::EvalEnvHandle>,
        effective_args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        self.record_runtime_bytecode_call();
        if stack.len() as u32 >= self.max_stack_depth {
            return Err(VmError::StackOverflow {
                limit: self.max_stack_depth,
            });
        }
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        // Async-call entry path (spec §27.7.5.1): synthesise a
        // fresh pending result promise, write it into the caller's
        // `dst` register *now* so the call expression's value is
        // visible synchronously, and park the new frame with
        // `return_register = None` so its eventual completion
        // settles the promise instead of writing back.
        let (return_register, async_state) = if function.is_async && !function.is_generator {
            let result_promise = promise_dispatch::PromiseBuilder::with_context(context.clone())
                .pending_stack_rooted(
                    self,
                    stack,
                    &[&this_for_callee],
                    &[effective_args.as_slice()],
                )?;
            let promise_value = Value::promise(result_promise);
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, promise_value)?;
            (None, Some(AsyncFrameState { result_promise }))
        } else {
            (Some(dst), None)
        };
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        let this_for_callee = self.this_for_bytecode_call_stack_rooted(
            function,
            stack,
            this_for_callee,
            &[effective_args.as_slice()],
        )?;
        let mut new_frame = Frame::with_exec_return_upvalues_and_this(
            function,
            return_register,
            upvalues,
            this_for_callee,
        );
        new_frame.async_state = async_state;
        if let Some(new_target) = new_target_for_callee {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.new_target = Some(new_target);
        }
        if let Some(cell) = derived_this_cell {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.derived_this_cell = Some(cell);
        }
        self.stash_frame_eval_env(function, &mut new_frame, callee_eval_env)?;
        self.bind_bytecode_call_arguments(function, &mut new_frame, effective_args)?;
        // §27.5 Generator-call entry: instead of pushing the frame
        // onto the dispatch stack, hand the caller a paused
        // [`Value::Generator`] handle that owns the prepared frame.
        // The body only runs when `.next()` resumes it.
        if function.is_generator {
            new_frame.return_register = None;
            let async_gen = function.is_async_generator;
            let generator_function_id = function.id;
            let gen_handle = crate::generator::JsGenerator::new_with_prototype(
                &mut self.gc_heap,
                new_frame,
                None,
            )?;
            gen_handle.set_async(&mut self.gc_heap, async_gen);
            // Backlink the generator into the frame so `Op::Yield`
            // can find its owner once execution starts.
            gen_handle.install_owner_on_frame(&mut self.gc_heap);
            let (frame, cold) = gen_handle
                .take_frame(&mut self.gc_heap)
                .ok_or(VmError::InvalidOperand)?;
            let mut frame = *frame;
            if let Some(cold) = cold {
                self.frame_attach_cold(&mut frame, cold);
            }
            let mut prologue_stack: SmallVec<[Frame; 8]> = SmallVec::new();
            prologue_stack.push(frame);
            self.dispatch_loop(context, &mut prologue_stack)?;
            self.resolve_generator_prototype_stack_rooted(
                context,
                stack,
                generator_function_id,
                &gen_handle,
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, Value::generator(gen_handle))?;
            return Ok(());
        }
        stack.push(new_frame);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_bytecode_call_frame_from_window(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        function_id: u32,
        parent_upvalues: UpvalueSpine,
        this_for_callee: Value,
        new_target_for_callee: Option<Value>,
        derived_this_cell: Option<crate::UpvalueCell>,
        callee_eval_env: Option<crate::eval_env::EvalEnvHandle>,
        args: &BytecodeArgumentWindow<'_>,
        return_register: Option<u16>,
        async_state: Option<AsyncFrameState>,
    ) -> Result<PreparedBytecodeFrame, VmError> {
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        let this_for_callee =
            self.this_for_bytecode_call_stack_rooted(function, stack, this_for_callee, &[])?;
        let mut frame = Frame::with_exec_return_upvalues_and_this(
            function,
            return_register,
            upvalues,
            this_for_callee,
        );
        frame.async_state = async_state;
        let extras = args.bind_into(function, &mut frame)?;
        if !extras.is_empty() {
            let cold = self.frame_ensure_cold(&mut frame);
            cold.rest_args = extras.rest_args;
            cold.incoming_args = extras.incoming_args;
        }
        if let Some(new_target) = new_target_for_callee {
            let cold = self.frame_ensure_cold(&mut frame);
            cold.new_target = Some(new_target);
        }
        if let Some(cell) = derived_this_cell {
            let cold = self.frame_ensure_cold(&mut frame);
            cold.derived_this_cell = Some(cell);
        }
        self.stash_frame_eval_env(function, &mut frame, callee_eval_env)?;
        Ok(PreparedBytecodeFrame {
            frame,
            is_generator: function.is_generator,
            is_async_generator: function.is_async_generator,
            generator_function_id: function_id,
        })
    }

    /// §27.5.1 step 3 / §9.1.14 — resolve a fresh generator's
    /// [[Prototype]] from `fn.prototype` AFTER the prologue ran; a
    /// non-object answer falls back (override `None`) to the realm's
    /// shared `%GeneratorPrototype%` / `%AsyncGeneratorPrototype%`.
    fn resolve_generator_prototype_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        function_id: u32,
        gen_handle: &crate::generator::JsGenerator,
    ) -> Result<(), VmError> {
        // Generator `.prototype` flows only the template id this deep in
        // dispatch; the per-instance owner is not threaded here (generator
        // closures keep template-keyed prototype materialization).
        let proto = self.function_property_get_stack_rooted(
            context,
            stack,
            None,
            function_id,
            "prototype",
        )?;
        gen_handle.set_prototype_override(
            &mut self.gc_heap,
            proto.as_object().is_some().then_some(proto),
        );
        Ok(())
    }

    fn push_prepared_bytecode_call_frame(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        dst: u16,
        prepared: PreparedBytecodeFrame,
    ) -> Result<(), VmError> {
        let PreparedBytecodeFrame {
            mut frame,
            is_generator,
            is_async_generator,
            generator_function_id,
        } = prepared;
        if is_generator {
            frame.return_register = None;
            let gen_handle =
                crate::generator::JsGenerator::new_with_prototype(&mut self.gc_heap, frame, None)?;
            gen_handle.set_async(&mut self.gc_heap, is_async_generator);
            gen_handle.install_owner_on_frame(&mut self.gc_heap);
            let (frame, cold) = gen_handle
                .take_frame(&mut self.gc_heap)
                .ok_or(VmError::InvalidOperand)?;
            let mut frame = *frame;
            if let Some(cold) = cold {
                self.frame_attach_cold(&mut frame, cold);
            }
            let mut prologue_stack: SmallVec<[Frame; 8]> = SmallVec::new();
            prologue_stack.push(frame);
            self.dispatch_loop(context, &mut prologue_stack)?;
            self.resolve_generator_prototype_stack_rooted(
                context,
                stack,
                generator_function_id,
                &gen_handle,
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, Value::generator(gen_handle))?;
            return Ok(());
        }
        stack.push(frame);
        Ok(())
    }

    fn try_push_bytecode_call_frame_from_window(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        operands: &[Operand],
        first_arg_operand: usize,
        argc: usize,
        dst: u16,
    ) -> Result<bool, VmError> {
        let current = *callee;
        let effective_this = this_value;
        if current.as_class_constructor().is_some() {
            // §10.3.1 — a class constructor's [[Call]] always
            // throws; only [[Construct]] may enter it.
            return Err(VmError::TypeError {
                message: "Class constructor cannot be invoked without 'new'".to_string(),
            });
        }
        if current.is_bound_function() {
            return Ok(false);
        }
        if !current.is_function() && !current.is_closure() {
            return Ok(false);
        }
        let (
            function_id,
            parent_upvalues,
            this_for_callee,
            new_target_for_callee,
            derived_this_cell,
            callee_eval_env,
        ) = Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap)?;
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        self.record_runtime_bytecode_call();
        if stack.len() as u32 >= self.max_stack_depth {
            return Err(VmError::StackOverflow {
                limit: self.max_stack_depth,
            });
        }
        let top_idx = stack.len() - 1;
        let (return_register, async_state) = if function.is_async && !function.is_generator {
            let result_promise = promise_dispatch::PromiseBuilder::with_context(context.clone())
                .pending_stack_rooted(self, stack, &[&this_for_callee], &[])?;
            let promise_value = Value::promise(result_promise);
            write_register(&mut stack[top_idx], dst, promise_value)?;
            (None, Some(AsyncFrameState { result_promise }))
        } else {
            (Some(dst), None)
        };
        let prepared = {
            let caller = &stack[top_idx];
            let args = BytecodeArgumentWindow::new(caller, operands, first_arg_operand, argc);
            self.prepare_bytecode_call_frame_from_window(
                context,
                stack,
                function_id,
                parent_upvalues,
                this_for_callee,
                new_target_for_callee,
                derived_this_cell,
                callee_eval_env,
                &args,
                return_register,
                async_state,
            )?
        };
        self.push_prepared_bytecode_call_frame(stack, context, dst, prepared)?;
        Ok(true)
    }

    fn try_invoke_native_call_from_window(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        operands: &[Operand],
        first_arg_operand: usize,
        argc: usize,
        dst: u16,
    ) -> Result<bool, VmError> {
        let top_idx = stack.len() - 1;
        let args = {
            let caller = &stack[top_idx];
            let window = BytecodeArgumentWindow::new(caller, operands, first_arg_operand, argc);
            let Some(args) = window.contiguous_slice()? else {
                return Ok(false);
            };
            args
        };

        if let Some(obj) = callee.as_object()
            && let Some(native) =
                crate::object::call_native(obj, &self.gc_heap).and_then(|v| v.as_native_function())
        {
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(_) = call {
                return Ok(false);
            }
            self.record_runtime_native_call();
            let result =
                invoke_native_call_with_roots(self, context, call, this_value, &[callee], args)?;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(true);
        }

        if let Some(native) = callee.as_native_function() {
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(_) = call {
                return Ok(false);
            }
            self.record_runtime_native_call();
            let result =
                invoke_native_call_with_roots(self, context, call, this_value, &[callee], args)?;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Handle `Op::Call`: push a new frame for the callee with
    /// arguments copied into the parameter slots and `this` bound
    /// to `Value::undefined()` (foundation strict default).
    pub(crate) fn do_call(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let argc = match operands.get(2) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };

        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        stack[top_idx].advance_pc(self.current_byte_len)?;
        if self.try_push_bytecode_call_frame_from_window(
            stack,
            context,
            &callee,
            Value::undefined(),
            operands,
            3,
            argc as usize,
            dst,
        )? {
            return Ok(());
        }
        if self.try_invoke_native_call_from_window(
            stack,
            context,
            &callee,
            Value::undefined(),
            operands,
            3,
            argc as usize,
            dst,
        )? {
            return Ok(());
        }
        let args = BytecodeArgumentWindow::new(&stack[top_idx], operands, 3, argc as usize)
            .to_smallvec8()?;
        self.invoke(stack, context, &callee, Value::undefined(), args, dst)
    }

    /// Invoke `callee` with the explicit receiver `this_value` and
    /// the given argument list. Centralizes the BoundFunction
    /// unwrapping, closure `bound_this` override, and frame push so
    /// every call opcode (`Op::Call`, `Op::CallWithThis`,
    /// `Op::CallMethodValue`) shares one path.
    ///
    /// `dst` is the **caller's** register that should receive the
    /// completion value when the callee returns. `caller_pc` must
    /// already be advanced before this call so the post-pop
    /// dispatch resumes after the originating instruction.
    pub(crate) fn invoke(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        // Walk through any number of `bind` layers, accumulating
        // their bound arguments and overriding `this_value` with
        // the innermost `bound_this`. The loop bound matches the
        // JS-call stack-depth limit so a pathological self-bound
        // chain still surfaces as `StackOverflow` rather than
        // unbounded recursion.
        let mut current = *callee;
        let mut effective_this = this_value;
        let mut effective_args = args;
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            if let Some(bound) = current.as_bound_function() {
                hops += 1;
                let (target, bound_this, bound_args) = bound.parts(&self.gc_heap);
                let mut combined: SmallVec<[Value; 8]> =
                    SmallVec::with_capacity(bound_args.len() + effective_args.len());
                combined.extend(bound_args);
                combined.extend(effective_args);
                effective_this = bound_this;
                effective_args = combined;
                current = target;
                continue;
            }
            if current.as_class_constructor().is_some() {
                // §10.2.1.1 / §10.3.1 — a class constructor's [[Call]]
                // always throws, including when reached through a
                // bound-function wrapper or Reflect/Function call
                // forwarding. Only [[Construct]] may enter it.
                return Err(VmError::TypeError {
                    message: "Class constructor cannot be invoked without 'new'".to_string(),
                });
            }
            break;
        }
        if current.is_function() || current.is_closure() {
            let (
                function_id,
                parent_upvalues,
                this_for_callee,
                new_target_for_callee,
                derived_this_cell,
                callee_eval_env,
            ) = Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap)?;
            return self.push_bytecode_call_frame(
                stack,
                context,
                function_id,
                parent_upvalues,
                this_for_callee,
                new_target_for_callee,
                derived_this_cell,
                callee_eval_env,
                effective_args,
                dst,
            );
        }
        // Native callables short-circuit the frame push: invoke
        // the closure inline, write the result into the caller's
        // dst, and advance pc on the caller frame. No stack frame
        // is created — the closure cannot itself push frames.
        if let Some(obj) = current.as_object()
            && let Some(native) =
                crate::object::call_native(obj, &self.gc_heap).and_then(|v| v.as_native_function())
        {
            let call = native.call_target(&self.gc_heap);
            self.record_runtime_native_call();
            let result = invoke_native_call_with_roots(
                self,
                context,
                call,
                effective_this,
                &[&current],
                effective_args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        if let Some(native) = current.as_native_function() {
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(intrinsic) = call {
                let result =
                    self.run_vm_intrinsic_sync(context, intrinsic, effective_this, effective_args)?;
                let top_idx = stack.len() - 1;
                write_register(&mut stack[top_idx], dst, result)?;
                return Ok(());
            }
            self.record_runtime_native_call();
            let result = invoke_native_call_with_roots(
                self,
                context,
                call,
                effective_this,
                &[&current],
                effective_args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        // §28.2.4.13 Proxy.[[Call]] — delegate to the `apply`
        // trap when present; otherwise call through to the
        // target as a function.
        if let Some(proxy) = current.as_proxy() {
            let argv_array = self.alloc_stack_rooted_array_from_values(
                stack,
                effective_args.iter().cloned(),
                &[&current, &effective_this],
                effective_args.as_slice(),
            )?;
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(&self.gc_heap),
                effective_this,
                Value::array(argv_array),
            ];
            let result = match self.invoke_proxy_trap(context, &proxy, "apply", trap_args)? {
                Some(v) => v,
                None => {
                    // Fall through to the target's [[Call]] —
                    // `proxy.target(&self.gc_heap)` returns the original Value,
                    // which may be a callable directly.
                    let underlying = proxy.target(&self.gc_heap);
                    self.run_callable_sync(context, &underlying, effective_this, effective_args)?
                }
            };
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        let (
            function_id,
            parent_upvalues,
            this_for_callee,
            new_target_for_callee,
            derived_this_cell,
            callee_eval_env,
        ) = Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap)?;
        self.push_bytecode_call_frame(
            stack,
            context,
            function_id,
            parent_upvalues,
            this_for_callee,
            new_target_for_callee,
            derived_this_cell,
            callee_eval_env,
            effective_args,
            dst,
        )
    }

    /// Handle `Op::New`: allocate a fresh receiver, set its
    /// `[[Prototype]]` to `callee.prototype` (when present), and
    /// invoke the callee with `this = receiver`. The caller's `dst`
    /// register receives either the constructor's returned object
    /// or the freshly allocated receiver — `pop_frame` performs
    /// that swap so the unwind path is uniform across call shapes.
    pub(crate) fn do_construct(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let argc = match operands.get(2) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        if !is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Err(VmError::NotCallable);
        }
        stack[top_idx].advance_pc(self.current_byte_len)?;
        if self.try_dispatch_construct_from_window(
            stack,
            context,
            callee,
            operands,
            3,
            argc as usize,
            dst,
        )? {
            return Ok(());
        }
        let args = BytecodeArgumentWindow::new(&stack[top_idx], operands, 3, argc as usize)
            .to_smallvec8()?;
        self.dispatch_construct(stack, context, callee, args, dst)
    }

    fn try_dispatch_construct_from_window(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        callee: Value,
        operands: &[Operand],
        first_arg_operand: usize,
        argc: usize,
        dst: u16,
    ) -> Result<bool, VmError> {
        let mut current = callee;
        let effective_new_target = current;
        if let Some(class) = current.as_class_constructor() {
            current = class.ctor(&self.gc_heap);
        }
        if !current.is_function() && !current.is_closure() {
            return Ok(false);
        }

        self.record_runtime_construct_call();
        let proto = self.construct_prototype_for_callee_stack_rooted(
            context,
            stack,
            &effective_new_target,
        )?;
        // OrdinaryCreateFromConstructor — a missing or non-object
        // `prototype` falls back to %Object.prototype% (§10.1.13).
        let proto = match proto {
            Some(proto) => proto,
            None => self.constructor_prototype_value("Object")?,
        };
        let receiver = self.alloc_stack_rooted_object_with_extra_roots(stack, &[&proto])?;
        crate::object::set_prototype_value(receiver, &mut self.gc_heap, Some(proto));
        let top_idx = stack.len() - 1;
        let frame = {
            let caller = &stack[top_idx];
            let args = BytecodeArgumentWindow::new(caller, operands, first_arg_operand, argc);
            self.build_construct_bytecode_frame_from_window(
                context,
                current,
                receiver,
                effective_new_target,
                &args,
                Some(dst),
            )?
        };
        stack.push(frame);
        Ok(true)
    }

    pub(crate) fn do_construct_spread(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let args_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        if !is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Err(VmError::NotCallable);
        }
        let args_value = *read_register(&stack[top_idx], args_reg)?;
        let Some(arr) = args_value.as_array() else {
            return Err(VmError::TypeMismatch);
        };
        let args: SmallVec<[Value; 8]> =
            crate::array::with_elements(arr, &self.gc_heap, |elements| {
                elements.iter().cloned().collect()
            });
        stack[top_idx].advance_pc(self.current_byte_len)?;
        self.dispatch_construct(stack, context, callee, args, dst)
    }

    pub(crate) fn do_super_construct_spread(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let args_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        if !is_constructor_runtime(&callee, context, &self.gc_heap) {
            return Err(VmError::NotCallable);
        }
        let new_target = self
            .frame_cold(&stack[top_idx])
            .and_then(|c| c.new_target)
            .unwrap_or(callee);
        let args_value = *read_register(&stack[top_idx], args_reg)?;
        let Some(arr) = args_value.as_array() else {
            return Err(VmError::TypeMismatch);
        };
        let args: SmallVec<[Value; 8]> =
            crate::array::with_elements(arr, &self.gc_heap, |elements| {
                elements.iter().cloned().collect()
            });
        stack[top_idx].advance_pc(self.current_byte_len)?;
        self.dispatch_construct_with_new_target(stack, context, callee, new_target, args, dst)
    }

    pub(crate) fn dispatch_construct(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        callee: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        self.dispatch_construct_with_new_target(stack, context, callee, callee, args, dst)
    }

    fn dispatch_construct_with_new_target(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        callee: Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        self.record_runtime_construct_call();
        let mut callee = callee;
        let mut new_target = new_target;
        let mut args = args;
        let mut hops: u32 = 0;
        while let Some(bound) = callee.as_bound_function() {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            hops += 1;
            let (target, _bound_this, bound_args) = bound.parts(&self.gc_heap);
            let mut combined: SmallVec<[Value; 8]> =
                SmallVec::with_capacity(bound_args.len() + args.len());
            combined.extend(bound_args);
            combined.extend(args);
            if abstract_ops::same_value(&callee, &new_target, &self.gc_heap) {
                new_target = target;
            }
            callee = target;
            args = combined;
        }
        // §28.2.4.14 Proxy.[[Construct]] — `new <proxy>(args)`
        // routes through the `construct` trap when present;
        // otherwise delegates to the target.
        if let Some(proxy) = callee.as_proxy() {
            let argv_array = self.alloc_stack_rooted_array_from_values(
                stack,
                args.iter().cloned(),
                &[&callee, &new_target],
                args.as_slice(),
            )?;
            let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                proxy.target(&self.gc_heap),
                Value::array(argv_array),
                new_target,
            ];
            let result = match self.invoke_proxy_trap(context, &proxy, "construct", trap_args)? {
                Some(v) => {
                    // §10.5.13 step 9 — trap result must be an Object;
                    // primitive returns surface as TypeError.
                    if !v.is_object_type() {
                        return Err(VmError::TypeError {
                            message: "Proxy construct trap returned non-object".to_string(),
                        });
                    }
                    v
                }
                None => {
                    // Fall through to [[Construct]] on the underlying
                    // target via `run_construct_sync`, which honours
                    // bound/proxy/native paths and re-checks the
                    // constructor-return invariants.
                    self.run_construct_sync(
                        context,
                        &proxy.target(&self.gc_heap),
                        new_target,
                        args,
                    )?
                }
            };
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        if let Some(native) = self.native_promise_constructor(&callee) {
            let constructed = self.invoke_native_construct(
                context,
                native,
                &Value::undefined(),
                &new_target,
                args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        // Allocate receiver and link its prototype before pushing
        // the new frame. The constructor might mutate the receiver
        // immediately, so the prototype link must already be in
        // place.
        let proto =
            self.construct_prototype_for_callee_stack_rooted(context, stack, &new_target)?;
        // OrdinaryCreateFromConstructor — a missing or non-object
        // `prototype` falls back to %Object.prototype% (§10.1.13).
        let proto = match proto {
            Some(proto) => proto,
            None => self.constructor_prototype_value("Object")?,
        };
        let receiver = {
            let value_roots: SmallVec<[&Value; 4]> =
                smallvec::smallvec![&callee, &new_target, &proto];
            self.alloc_stack_rooted_object_with_value_roots(stack, value_roots.as_slice(), &args)?
        };
        crate::object::set_prototype_value(receiver, &mut self.gc_heap, Some(proto));
        let this_value = Value::object(receiver);
        // Built-in constructor objects (`Number`, `Boolean`, …)
        // surface as a `Value::Object` with an internal native
        // constructor slot. Promote to the native-function construct
        // path so the JS-visible callee can also carry own
        // properties (statics + `prototype`) without leaking the
        // implementation slot through reflection.
        if let Some(obj) = callee.as_object()
            && let Some(native) = crate::object::constructor_native(obj, &self.gc_heap)
                .and_then(|v| v.as_native_function())
        {
            let constructed = self.invoke_native_construct(
                context,
                native,
                &this_value,
                &new_target,
                args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        // `Value::NativeFunction` carries `[[Construct]]` whenever
        // the runtime needs the callable to behave as a constructor
        // (e.g. `new Number(x)`). The native callback inspects
        // `NativeCtx::is_construct_call()` to differentiate the
        // call shape.
        if let Some(native) = callee.as_native_function() {
            let constructed = self.invoke_native_construct(
                context,
                native,
                &this_value,
                &new_target,
                args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        if let Some(class) = callee.as_class_constructor()
            && let Some(native) = class.ctor(&self.gc_heap).as_native_function()
        {
            let constructed = self.invoke_native_construct(
                context,
                native,
                &this_value,
                &new_target,
                args.as_slice(),
            )?;
            let top_idx = stack.len() - 1;
            write_register(&mut stack[top_idx], dst, constructed)?;
            return Ok(());
        }
        let bytecode_callee = if let Some(class) = callee.as_class_constructor() {
            class.ctor(&self.gc_heap)
        } else {
            callee
        };
        if bytecode_callee.is_function() || bytecode_callee.is_closure() {
            let frame = self.build_construct_bytecode_frame(
                context,
                bytecode_callee,
                receiver,
                new_target,
                args,
                Some(dst),
            )?;
            stack.push(frame);
            return Ok(());
        }
        self.invoke(stack, context, &callee, this_value, args, dst)?;
        // The pushed frame is now on top; mark it so `pop_frame`
        // can substitute the receiver for any non-object return.
        if let Some(top) = stack.last_mut() {
            let cold = self.frame_ensure_cold(top);
            cold.construct_target = Some(receiver);
            cold.new_target = Some(new_target);
        }
        Ok(())
    }

    pub(crate) fn construct_prototype_for_callee_stack_rooted(
        &mut self,
        context: &ExecutionContext,
        stack: &SmallVec<[Frame; 8]>,
        callee: &Value,
    ) -> Result<Option<Value>, VmError> {
        let function_id = callee.as_function().or_else(|| {
            callee
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = function_id {
            let owner = callee.as_closure(&self.gc_heap);
            return match self.function_property_get_stack_rooted_with_receiver(
                context,
                stack,
                owner,
                function_id,
                Some(*callee),
                "prototype",
            )? {
                proto if proto.is_object_type() => Ok(Some(proto)),
                _ => Ok(None),
            };
        }
        if let Some(c) = callee.as_class_constructor() {
            return Ok(Some(Value::object(c.prototype(&self.gc_heap))));
        }
        if callee.as_proxy().is_some() {
            return self.construct_prototype_via_get(context, callee);
        }
        if let Some(obj) = callee.as_object() {
            return Ok(match crate::object::get(obj, &self.gc_heap, "prototype") {
                Some(proto) if proto.is_object_type() => Some(proto),
                _ => None,
            });
        }
        if callee.is_bound_function() {
            return self.construct_prototype_via_get(context, callee);
        }
        if let Some(native) = callee.as_native_function() {
            return native
                .own_property_descriptor(&mut self.gc_heap, "prototype")
                .map_err(|_| VmError::InvalidOperand)
                .map(|desc| {
                    desc.and_then(|d| match d.kind {
                        crate::object::DescriptorKind::Data { value } if value.is_object_type() => {
                            Some(value)
                        }
                        _ => None,
                    })
                });
        }
        Ok(None)
    }

    pub(crate) fn construct_prototype_for_callee_runtime_rooted(
        &mut self,
        context: &ExecutionContext,
        callee: &Value,
        value_roots: &[&Value],
        slice_roots: &[&[Value]],
    ) -> Result<Option<Value>, VmError> {
        let function_id = callee.as_function().or_else(|| {
            callee
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = function_id {
            let owner = callee.as_closure(&self.gc_heap);
            return match self.function_property_get_runtime_rooted_with_receiver(
                context,
                owner,
                function_id,
                Some(*callee),
                "prototype",
                value_roots,
                slice_roots,
            )? {
                proto if proto.is_object_type() => Ok(Some(proto)),
                _ => Ok(None),
            };
        }
        if let Some(c) = callee.as_class_constructor() {
            return Ok(Some(Value::object(c.prototype(&self.gc_heap))));
        }
        if callee.as_proxy().is_some() {
            return self.construct_prototype_via_get(context, callee);
        }
        if let Some(obj) = callee.as_object() {
            return Ok(match crate::object::get(obj, &self.gc_heap, "prototype") {
                Some(proto) if proto.is_object_type() => Some(proto),
                _ => None,
            });
        }
        if callee.is_bound_function() {
            return self.construct_prototype_via_get(context, callee);
        }
        if let Some(native) = callee.as_native_function() {
            return native
                .own_property_descriptor(&mut self.gc_heap, "prototype")
                .map_err(|_| VmError::InvalidOperand)
                .map(|desc| {
                    desc.and_then(|d| match d.kind {
                        crate::object::DescriptorKind::Data { value } if value.is_object_type() => {
                            Some(value)
                        }
                        _ => None,
                    })
                });
        }
        Ok(None)
    }

    fn construct_prototype_via_get(
        &mut self,
        context: &ExecutionContext,
        callee: &Value,
    ) -> Result<Option<Value>, VmError> {
        let proxy = callee.as_proxy();
        let key = VmPropertyKey::String("prototype");
        let proto = match self.ordinary_get_value(context, *callee, *callee, &key, 0)? {
            VmGetOutcome::Value(value) => value,
            VmGetOutcome::InvokeGetter { getter } => {
                self.run_callable_sync(context, &getter, *callee, SmallVec::new())?
            }
        };
        if !proto.is_object_type() && proxy.is_some_and(|proxy| proxy.is_revoked(&self.gc_heap)) {
            return Err(VmError::TypeError {
                message: "Cannot get prototype from a revoked proxy".to_string(),
            });
        }
        Ok(proto.is_object_type().then_some(proto))
    }

    fn native_promise_constructor(&self, callee: &Value) -> Option<NativeFunction> {
        let native = if let Some(native) = callee.as_native_function() {
            native
        } else if let Some(obj) = callee.as_object() {
            crate::object::constructor_native(obj, &self.gc_heap)
                .and_then(|v| v.as_native_function())?
        } else {
            return None;
        };
        (native.name(&self.gc_heap) == "Promise").then_some(native)
    }

    /// Handle `Op::CallSpread`: read the args array, fan it out
    /// into the standard call path. The receiver register holds
    /// the explicit `this` value (foundation lowers free spread
    /// calls with `this = undefined`).
    pub(crate) fn do_call_spread(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let args_reg = register_operand(operands.get(3))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let this_value = *read_register(&stack[top_idx], this_reg)?;
        let args_array = read_register(&stack[top_idx], args_reg)?
            .as_array()
            .ok_or(VmError::TypeMismatch)?;
        let args: SmallVec<[Value; 8]> =
            crate::array::with_elements(args_array, &self.gc_heap, |elements| {
                elements.iter().cloned().collect()
            });
        stack[top_idx].advance_pc(self.current_byte_len)?;
        self.invoke(stack, context, &callee, this_value, args, dst)
    }

    /// Handle `Op::CallWithThis`: same as `do_call` but the call
    /// site supplies an explicit `this` register. Used by
    /// `Function.prototype.call` lowering and the array-literal
    /// path of `Function.prototype.apply`.
    pub(crate) fn do_call_with_this(
        &mut self,
        stack: &mut SmallVec<[Frame; 8]>,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let this_value = *read_register(&stack[top_idx], this_reg)?;
        stack[top_idx].advance_pc(self.current_byte_len)?;
        if self.try_push_bytecode_call_frame_from_window(
            stack,
            context,
            &callee,
            this_value,
            operands,
            4,
            argc as usize,
            dst,
        )? {
            return Ok(());
        }
        if self.try_invoke_native_call_from_window(
            stack,
            context,
            &callee,
            this_value,
            operands,
            4,
            argc as usize,
            dst,
        )? {
            return Ok(());
        }
        let args = BytecodeArgumentWindow::new(&stack[top_idx], operands, 4, argc as usize)
            .to_smallvec8()?;
        self.invoke(stack, context, &callee, this_value, args, dst)
    }
    /// Synchronously invoke `callee(args)` with the given `this` and
    /// return the completion value.
    ///
    /// # Algorithm
    /// 1. NativeFunction callees run inline — the foundation native
    ///    surface is `Fn`, so calling them here is just a function
    ///    pointer hop with `&mut self` access.
    /// 2. BoundFunction layers are unwrapped iteratively, prepending
    ///    bound args and replacing `this_value` with `bound_this`.
    /// 3. Bytecode / closure callees push a frame whose
    ///    `return_register` is `None`, which makes
    ///    [`Self::dispatch_loop`] return the completion value when
    ///    the frame pops.
    ///
    /// Used by collection `forEach` and other host-driven iteration
    /// helpers.
    pub fn run_callable_sync(
        &mut self,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        self.enter_sync_reentry()?;
        // Host callers (timer fire, worker message dispatch) enter
        // here without `Interpreter::run`'s rooted scope, and the
        // inner body allocates (upvalue spines, `this` boxing) before
        // `dispatch_loop` registers frame roots — so the runtime
        // roots must be registered for the whole call.
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_root_depth = self.gc_heap.push_extra_roots(extra_roots);
        let result = self.run_callable_sync_inner(context, callee, this_value, args);
        self.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
        self.leave_sync_reentry();
        result
    }

    fn run_callable_sync_inner(
        &mut self,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let mut current = *callee;
        let mut effective_this = this_value;
        let mut effective_args = args;
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            if let Some(bound) = current.as_bound_function() {
                hops += 1;
                let (target, bound_this, bound_args) = bound.parts(&self.gc_heap);
                let mut combined: SmallVec<[Value; 8]> =
                    SmallVec::with_capacity(bound_args.len() + effective_args.len());
                combined.extend(bound_args);
                combined.extend(effective_args);
                effective_this = bound_this;
                effective_args = combined;
                current = target;
            } else if current.as_class_constructor().is_some() {
                // §10.3.1 — class constructors reject [[Call]].
                return Err(VmError::TypeError {
                    message: "Class constructor cannot be invoked without 'new'".to_string(),
                });
            } else if let Some(proxy) = current.as_proxy() {
                // §10.5.12 Proxy [[Call]] — dispatch `apply` trap or
                // fall through to target.[[Call]] when the trap is
                // absent.
                if proxy.is_revoked(&self.gc_heap) {
                    return Err(VmError::TypeError {
                        message: "Cannot perform 'apply' on a proxy that has been revoked"
                            .to_string(),
                    });
                }
                hops += 1;
                let handler = proxy.handler(&self.gc_heap);
                let trap_key = VmPropertyKey::String("apply");
                let trap_value =
                    match self.ordinary_get_value(context, handler, handler, &trap_key, 0)? {
                        VmGetOutcome::Value(value) => value,
                        VmGetOutcome::InvokeGetter { getter } => {
                            self.run_callable_sync(context, &getter, handler, SmallVec::new())?
                        }
                    };
                if self.is_callable_runtime(&trap_value) {
                    let argv_array = self.alloc_runtime_rooted_array_from_values(
                        effective_args.iter().cloned(),
                        &[&current, &effective_this, &handler, &trap_value],
                        &[effective_args.as_slice()],
                    )?;
                    let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                        proxy.target(&self.gc_heap),
                        effective_this,
                        Value::array(argv_array),
                    ];
                    return self.run_callable_sync(context, &trap_value, handler, trap_args);
                } else if trap_value.is_undefined() || trap_value.is_null() {
                    current = proxy.target(&self.gc_heap);
                } else {
                    return Err(VmError::TypeError {
                        message: "Proxy apply trap is not callable".to_string(),
                    });
                }
            } else {
                break;
            }
        }
        if let Some(obj) = current.as_object()
            && let Some(native) =
                crate::object::call_native(obj, &self.gc_heap).and_then(|v| v.as_native_function())
        {
            let call = native.call_target(&self.gc_heap);
            self.record_runtime_native_call();
            return invoke_native_call_with_roots(
                self,
                context,
                call,
                effective_this,
                &[&current],
                effective_args.as_slice(),
            );
        }
        if let Some(native) = current.as_native_function() {
            let native = &native;
            let call = native.call_target(&self.gc_heap);
            if let crate::native_function::NativeCallTarget::VmIntrinsic(intrinsic) = call {
                return self.run_vm_intrinsic_sync(
                    context,
                    intrinsic,
                    effective_this,
                    effective_args,
                );
            }
            self.record_runtime_native_call();
            return invoke_native_call_with_roots(
                self,
                context,
                call,
                effective_this,
                &[&current],
                effective_args.as_slice(),
            );
        }
        let (
            function_id,
            parent_upvalues,
            this_for_callee,
            new_target_for_callee,
            derived_this_cell,
            callee_eval_env,
        ) = Self::bytecode_call_target_parts(current, effective_this, &self.gc_heap)?;
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let upvalues =
            Frame::build_upvalues_for_exec(&mut self.gc_heap, function, parent_upvalues)?;
        let this_for_callee = self.this_for_bytecode_call_runtime_rooted(
            function,
            this_for_callee,
            &[effective_args.as_slice()],
        )?;
        let mut inner: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut new_frame =
            Frame::with_exec_return_upvalues_and_this(function, None, upvalues, this_for_callee);
        // A closure frame records its instance so the named-function SELF
        // binding inside the body resolves to it (per-instance `.prototype`).
        if let Some(closure) = current.as_closure(&self.gc_heap) {
            self.frame_ensure_cold(&mut new_frame).callee_closure = Some(closure);
        }
        if let Some(new_target) = new_target_for_callee {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.new_target = Some(new_target);
        }
        if let Some(cell) = derived_this_cell {
            let cold = self.frame_ensure_cold(&mut new_frame);
            cold.derived_this_cell = Some(cell);
        }
        self.stash_frame_eval_env(function, &mut new_frame, callee_eval_env)?;
        self.bind_bytecode_call_arguments(function, &mut new_frame, effective_args)?;
        // §27.5.1 GeneratorFunction call evaluation returns a
        // generator object without executing the body. `invoke`
        // handles this for opcode calls; the synchronous re-entry
        // helper must mirror it for builtins that call user
        // functions, such as `GetSetRecord(...).[[Keys]]`.
        // <https://tc39.es/ecma262/#sec-generatorfunction-objects>
        if function.is_generator {
            new_frame.return_register = None;
            let async_gen = function.is_async_generator;
            let gen_handle = crate::generator::JsGenerator::new_with_prototype(
                &mut self.gc_heap,
                new_frame,
                None,
            )?;
            gen_handle.set_async(&mut self.gc_heap, async_gen);
            gen_handle.install_owner_on_frame(&mut self.gc_heap);
            // §27.5 — run the generator prologue (mirroring the opcode
            // `invoke` path) so the handle is primed to its
            // suspended-start state. Without it a generator created
            // through a builtin's synchronous re-entry (e.g. an
            // `@@iterator` that is a generator function, driven by
            // `Array.from` / `GetSetRecord` / the iterator helpers) is
            // never started and reports `done` on its first `next`.
            let (frame, cold) = gen_handle
                .take_frame(&mut self.gc_heap)
                .ok_or(VmError::InvalidOperand)?;
            let mut frame = *frame;
            if let Some(cold) = cold {
                self.frame_attach_cold(&mut frame, cold);
            }
            let mut prologue_stack: SmallVec<[Frame; 8]> = SmallVec::new();
            prologue_stack.push(frame);
            self.dispatch_loop(context, &mut prologue_stack)?;
            // §27.5.1 step 3 — resolve [[Prototype]] after the
            // prologue (FunctionDeclarationInstantiation) ran. Only the
            // template id flows this deep; per-instance owner not threaded.
            let proto = self.function_property_get_runtime_rooted(
                context,
                None,
                function_id,
                "prototype",
                &[],
                &[],
            )?;
            gen_handle.set_prototype_override(
                &mut self.gc_heap,
                proto.as_object().is_some().then_some(proto),
            );
            return Ok(Value::generator(gen_handle));
        }
        inner.push(new_frame);
        self.dispatch_loop(context, &mut inner)
    }

    /// Synchronously perform `Construct(target, args, newTarget)`.
    ///
    /// This mirrors the `Op::New` user-function entry path but
    /// returns the completion directly for builtins such as
    /// `Reflect.construct`. Bound functions are unwrapped with the
    /// ECMA-262 `[[Construct]]` newTarget rewrite: constructing a
    /// bound function as itself exposes the bound target as
    /// `new.target` inside the target body.
    pub(crate) fn run_construct_sync(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        self.enter_sync_reentry()?;
        // Same rooting contract as [`Self::run_callable_sync`].
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_root_depth = self.gc_heap.push_extra_roots(extra_roots);
        let result = self.run_construct_sync_inner(context, target, new_target, args);
        self.gc_heap.pop_extra_roots_to(extra_root_depth - 1);
        self.leave_sync_reentry();
        result
    }

    fn run_construct_sync_inner(
        &mut self,
        context: &ExecutionContext,
        target: &Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        self.record_runtime_construct_call();
        let mut current = *target;
        let mut effective_new_target = new_target;
        let mut effective_args = args;
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            if let Some(bound) = current.as_bound_function() {
                hops += 1;
                let (next_target, _bound_this, bound_args) = bound.parts(&self.gc_heap);
                let mut combined: SmallVec<[Value; 8]> =
                    SmallVec::with_capacity(bound_args.len() + effective_args.len());
                combined.extend(bound_args);
                combined.extend(effective_args);
                if abstract_ops::same_value(&current, &effective_new_target, &self.gc_heap) {
                    effective_new_target = next_target;
                }
                current = next_target;
                effective_args = combined;
            } else if let Some(proxy) = current.as_proxy() {
                // §10.5.13 Proxy [[Construct]].
                if proxy.is_revoked(&self.gc_heap) {
                    return Err(VmError::TypeError {
                        message: "Cannot perform 'construct' on a proxy that has been revoked"
                            .to_string(),
                    });
                }
                hops += 1;
                let handler = proxy.handler(&self.gc_heap);
                let trap_key = VmPropertyKey::String("construct");
                let trap_value =
                    match self.ordinary_get_value(context, handler, handler, &trap_key, 0)? {
                        VmGetOutcome::Value(value) => value,
                        VmGetOutcome::InvokeGetter { getter } => {
                            self.run_callable_sync(context, &getter, handler, SmallVec::new())?
                        }
                    };
                if self.is_callable_runtime(&trap_value) {
                    let target_value = proxy.target(&self.gc_heap);
                    let argv_array = self.alloc_runtime_rooted_array_from_values(
                        effective_args.iter().cloned(),
                        &[
                            &current,
                            &target_value,
                            &effective_new_target,
                            &handler,
                            &trap_value,
                        ],
                        &[effective_args.as_slice()],
                    )?;
                    let trap_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                        target_value,
                        Value::array(argv_array),
                        effective_new_target,
                    ];
                    let result =
                        self.run_callable_sync(context, &trap_value, handler, trap_args)?;
                    if !result.is_object_type() {
                        return Err(VmError::TypeError {
                            message: "Proxy construct trap returned non-object".to_string(),
                        });
                    }
                    return Ok(result);
                } else if trap_value.is_undefined() || trap_value.is_null() {
                    current = proxy.target(&self.gc_heap);
                } else {
                    return Err(VmError::TypeError {
                        message: "Proxy construct trap is not callable".to_string(),
                    });
                }
            } else {
                break;
            }
        }

        if let Some(native) = self.native_promise_constructor(&current) {
            return self.invoke_native_construct(
                context,
                native,
                &Value::undefined(),
                &effective_new_target,
                effective_args.as_slice(),
            );
        }
        if let Some(native) = current.as_native_function()
            && matches!(
                native.name(&self.gc_heap),
                "ArrayBuffer"
                    | "SharedArrayBuffer"
                    | "DataView"
                    | "Int8Array"
                    | "Uint8Array"
                    | "Uint8ClampedArray"
                    | "Int16Array"
                    | "Uint16Array"
                    | "Int32Array"
                    | "Uint32Array"
                    | "Float16Array"
                    | "Float32Array"
                    | "Float64Array"
                    | "BigInt64Array"
                    | "BigUint64Array"
            )
        {
            return self.invoke_native_construct(
                context,
                native,
                &Value::undefined(),
                &effective_new_target,
                effective_args.as_slice(),
            );
        }

        let mut proto = self.construct_prototype_for_callee_runtime_rooted(
            context,
            &effective_new_target,
            &[&current, &effective_new_target],
            &[effective_args.as_slice()],
        )?;
        if proto.is_none()
            && current
                .as_native_function()
                .is_some_and(|native| native.name(&self.gc_heap) == "Date")
        {
            proto = Some(self.constructor_prototype_value("Date")?);
        }
        let receiver = {
            let mut value_roots: SmallVec<[&Value; 4]> =
                smallvec::smallvec![&current, &effective_new_target];
            if let Some(proto_value) = proto.as_ref() {
                value_roots.push(proto_value);
            }
            self.alloc_runtime_rooted_object_with_roots(
                value_roots.as_slice(),
                &[effective_args.as_slice()],
            )?
        };
        if let Some(proto) = proto {
            crate::object::set_prototype_value(receiver, &mut self.gc_heap, Some(proto));
        }
        let this_value = Value::object(receiver);

        if let Some(obj) = current.as_object()
            && let Some(native) = crate::object::constructor_native(obj, &self.gc_heap)
                .and_then(|v| v.as_native_function())
        {
            return self.invoke_native_construct(
                context,
                native,
                &this_value,
                &effective_new_target,
                effective_args.as_slice(),
            );
        }
        if let Some(native) = current.as_native_function() {
            return self.invoke_native_construct(
                context,
                native,
                &this_value,
                &effective_new_target,
                effective_args.as_slice(),
            );
        }
        if let Some(class) = current.as_class_constructor()
            && let Some(native) = class.ctor(&self.gc_heap).as_native_function()
        {
            return self.invoke_native_construct(
                context,
                native,
                &this_value,
                &effective_new_target,
                effective_args.as_slice(),
            );
        }
        if let Some(class) = current.as_class_constructor() {
            current = class.ctor(&self.gc_heap);
        }

        let new_frame = self.build_construct_bytecode_frame(
            context,
            current,
            receiver,
            effective_new_target,
            effective_args,
            None,
        )?;
        let mut inner: SmallVec<[Frame; 8]> = SmallVec::new();
        inner.push(new_frame);
        self.dispatch_loop(context, &mut inner)
    }
}
