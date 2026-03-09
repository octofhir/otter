use super::*;

// ============================================================================
// Generator Execution
// ============================================================================

/// Result of generator execution
#[derive(Debug)]
pub enum GeneratorResult {
    /// Generator yielded a value
    Yielded(Value),
    /// Generator returned a value (completed)
    Returned(Value),
    /// Generator threw an error
    Error(VmError),
    /// Async generator suspended on await (waiting for promise)
    Suspended {
        /// The promise being awaited
        promise: GcRef<JsPromise>,
        /// The register to store the resolved value
        resume_reg: u16,
        /// The generator (for resumption)
        generator: GcRef<JsGenerator>,
    },
}

pub(super) fn make_iterator_result_object(
    _memory_manager: Arc<crate::memory::MemoryManager>,
    value: Value,
    done: bool,
) -> Value {
    let iter_result = GcRef::new(JsObject::new(Value::null()));
    let _ = iter_result.set(PropertyKey::string("value"), value);
    let _ = iter_result.set(PropertyKey::string("done"), Value::boolean(done));
    Value::object(iter_result)
}

pub(super) fn make_async_generator_resume_callback(
    generator: GcRef<JsGenerator>,
    is_rejection: bool,
    memory_manager: Arc<crate::memory::MemoryManager>,
) -> Value {
    let native: NativeFn = Arc::new(move |_this, args, ncx| {
        let input = args.first().cloned().unwrap_or_else(Value::undefined);
        let gen_result = if is_rejection {
            generator.set_pending_throw(input);
            ncx.execute_generator(generator, None)
        } else {
            ncx.execute_generator(generator, Some(input))
        };
        Ok(async_generator_result_to_promise_value(
            gen_result,
            ncx.memory_manager().clone(),
            ncx.js_job_queue(),
        ))
    });
    Value::native_function_from_decl("__asyncGeneratorResume", native, 1, memory_manager)
}

pub(super) fn async_generator_result_to_promise_value(
    gen_result: GeneratorResult,
    memory_manager: Arc<crate::memory::MemoryManager>,
    js_queue: Option<Arc<dyn crate::context::JsJobQueueTrait + Send + Sync>>,
) -> Value {
    let promise = JsPromise::new();

    match gen_result {
        GeneratorResult::Yielded(v) => {
            let iter_result = make_iterator_result_object(memory_manager, v, false);
            if let Some(queue) = js_queue.clone() {
                JsPromise::resolve_with_js_jobs(promise, iter_result, move |job, args| {
                    queue.enqueue(job, args);
                });
            } else {
                promise.resolve(iter_result);
            }
        }
        GeneratorResult::Returned(v) => {
            let iter_result = make_iterator_result_object(memory_manager, v, true);
            if let Some(queue) = js_queue.clone() {
                JsPromise::resolve_with_js_jobs(promise, iter_result, move |job, args| {
                    queue.enqueue(job, args);
                });
            } else {
                promise.resolve(iter_result);
            }
        }
        GeneratorResult::Error(e) => {
            let error_value = Value::string(JsString::intern(&e.to_string()));
            if let Some(queue) = js_queue.clone() {
                JsPromise::reject_with_js_jobs(promise, error_value, move |job, args| {
                    queue.enqueue(job, args);
                });
            } else {
                promise.reject(error_value);
            }
        }
        GeneratorResult::Suspended {
            promise: awaited_promise,
            generator,
            ..
        } => {
            if let Some(queue) = js_queue.clone() {
                let fulfill_callback =
                    make_async_generator_resume_callback(generator, false, memory_manager.clone());
                let reject_callback =
                    make_async_generator_resume_callback(generator, true, memory_manager.clone());

                let result_promise = promise.clone();
                let queue_on_fulfill = queue.clone();
                awaited_promise.then(move |resolved_value| {
                    queue_on_fulfill.enqueue(
                        JsPromiseJob {
                            kind: JsPromiseJobKind::Fulfill,
                            callback: fulfill_callback.clone(),
                            this_arg: Value::undefined(),
                            result_promise: Some(result_promise.clone()),
                        },
                        vec![resolved_value],
                    );
                });

                let result_promise = promise.clone();
                let queue_on_reject = queue.clone();
                awaited_promise.catch(move |reason| {
                    queue_on_reject.enqueue(
                        JsPromiseJob {
                            kind: JsPromiseJobKind::Reject,
                            callback: reject_callback.clone(),
                            this_arg: Value::undefined(),
                            result_promise: Some(result_promise.clone()),
                        },
                        vec![reason],
                    );
                });
            } else {
                // Fallback for contexts without JS job queue: preserve old best-effort behavior.
                let result_promise = promise.clone();
                let mm = memory_manager.clone();
                awaited_promise.then(move |resolved_value| {
                    let iter_result = make_iterator_result_object(mm, resolved_value, false);
                    result_promise.resolve(iter_result);
                });
                let result_promise = promise.clone();
                awaited_promise.catch(move |reason| {
                    result_promise.reject(reason);
                });
            }
        }
    }

    Value::promise(promise)
}

impl Interpreter {
    /// Execute a generator (start or resume)
    ///
    /// This method handles both starting a generator for the first time
    /// and resuming a suspended generator.
    ///
    /// # Arguments
    /// * `generator` - The generator to execute
    /// * `ctx` - The VM context
    /// * `sent_value` - Value sent to the generator (for next(value))
    ///
    /// # Returns
    /// * `GeneratorResult::Yielded(value)` - Generator yielded
    /// * `GeneratorResult::Returned(value)` - Generator completed
    /// * `GeneratorResult::Error(err)` - Generator threw an error
    pub fn execute_generator(
        &self,
        generator: GcRef<JsGenerator>,
        ctx: &mut VmContext,
        sent_value: Option<Value>,
    ) -> GeneratorResult {
        match generator.state() {
            GeneratorState::Completed => {
                // Already completed - return undefined
                GeneratorResult::Returned(Value::undefined())
            }
            GeneratorState::Executing => {
                // Already executing - this is an error
                GeneratorResult::Error(VmError::type_error(
                    "Generator is already executing".to_string(),
                ))
            }
            GeneratorState::SuspendedStart => {
                // First execution - set up initial frame
                generator.start_executing();
                self.start_generator_execution(generator, ctx, sent_value)
            }
            GeneratorState::SuspendedYield => {
                // Resume from saved frame
                generator.start_executing();
                self.resume_generator_execution(generator, ctx, sent_value)
            }
        }
    }

    /// Start generator execution from the beginning
    fn start_generator_execution(
        &self,
        generator: GcRef<JsGenerator>,
        ctx: &mut VmContext,
        _sent_value: Option<Value>,
    ) -> GeneratorResult {
        // Handle pending throw (for generator.throw() called on a generator that hasn't started)
        if let Some(error) = generator.take_pending_throw() {
            generator.complete();
            return GeneratorResult::Error(VmError::exception(error));
        }

        // Handle pending return (for generator.return() called on a generator that hasn't started)
        if let Some(value) = generator.take_pending_return() {
            generator.complete();
            return GeneratorResult::Returned(value);
        }
        // Get generator's function info
        let func = match generator.module.function(generator.function_index) {
            Some(f) => f,
            None => {
                generator.complete();
                return GeneratorResult::Error(VmError::internal("Generator function not found"));
            }
        };

        // Take initial arguments
        let args = generator.take_initial_args();
        let this_value = generator.take_initial_this();
        let argc = args.len() as u16;

        // Attempt JIT execution for the generator body (bail-on-yield strategy).
        // This must happen before set_pending_args/this since those consume the values.
        if otter_vm_exec::is_jit_enabled() && func.jit_entry_ptr() != 0 && !func.is_deoptimized() {
            let jit_interp: *const Self = self;
            let jit_ctx_ptr: *mut VmContext = ctx;
            let upvalues = generator.upvalues.clone();
            ctx.set_pending_this(this_value.clone());
            match crate::jit_runtime::try_execute_jit(
                generator.module.module_id,
                generator.function_index,
                func,
                &args,
                ctx.cached_proto_epoch,
                jit_interp,
                jit_ctx_ptr,
                &generator.module.constants as *const _,
                &upvalues,
                None,
            ) {
                crate::jit_runtime::JitCallResult::Ok(value) => {
                    generator.complete();
                    return GeneratorResult::Returned(value);
                }
                crate::jit_runtime::JitCallResult::BailoutResume(state) => {
                    let try_stack = Self::reconstruct_try_stack(
                        func,
                        state.bailout_pc as usize,
                        ctx.stack_depth() + 1,
                    );
                    if let Some(resume) = crate::jit_resume::try_materialize_generator_yield(
                        ctx,
                        &state,
                        func,
                        generator.function_index,
                        Arc::clone(&generator.module),
                        upvalues,
                        try_stack,
                        this_value.clone(),
                        generator.is_construct(),
                        argc,
                    ) {
                        generator.suspend_with_frame(resume.frame);
                        return GeneratorResult::Yielded(resume.yielded_value);
                    }
                }
                crate::jit_runtime::JitCallResult::NeedsRecompilation => {
                    otter_vm_exec::enqueue_hot_function(
                        &generator.module,
                        generator.function_index,
                        func,
                    );
                    otter_vm_exec::compile_one_pending_request(
                        crate::jit_runtime::runtime_helpers(),
                    );
                }
                crate::jit_runtime::JitCallResult::BailoutRestart
                | crate::jit_runtime::JitCallResult::NotCompiled => {}
            }
        }

        // Set up pending args and push initial frame
        ctx.set_pending_realm_id(generator.realm_id);
        ctx.set_pending_args_from_vec(args);
        ctx.set_pending_this(this_value);
        ctx.set_pending_upvalues(generator.upvalues.clone());
        // Set callee value for arguments.callee in sloppy mode
        if let Some(callee) = generator.take_callee_value() {
            ctx.set_pending_callee_value(callee);
        }

        // Remember the stack depth before pushing the generator frame
        let initial_depth = ctx.stack_depth();

        ctx.register_module(&generator.module);
        if let Err(e) = ctx.push_frame(
            generator.function_index,
            generator.module.module_id,
            func.local_count,
            None,
            generator.is_construct(),
            false, // generators are not async
            argc,
        ) {
            generator.complete();
            return GeneratorResult::Error(e);
        }

        // Run until yield or return (with panic protection)
        {
            use std::panic::{AssertUnwindSafe, catch_unwind};
            match catch_unwind(AssertUnwindSafe(|| {
                self.run_generator_loop(generator, ctx, initial_depth)
            })) {
                Ok(result) => result,
                Err(panic_payload) => {
                    generator.complete();
                    GeneratorResult::Error(VmError::internal(&panic_message(&panic_payload)))
                }
            }
        }
    }

    /// Resume generator execution from saved frame
    fn resume_generator_execution(
        &self,
        generator: GcRef<JsGenerator>,
        ctx: &mut VmContext,
        sent_value: Option<Value>,
    ) -> GeneratorResult {
        // Get the saved frame
        let frame = match generator.take_frame() {
            Some(f) => f,
            None => {
                generator.complete();
                return GeneratorResult::Error(VmError::internal("Generator has no saved frame"));
            }
        };

        // Capture yield_dst and pending throw from frame
        let yield_dst = frame.yield_dst;
        let pending_throw = frame.pending_throw.clone();

        // Check for pending return (set on the generator, not the frame)
        // This is set by generator.return() and persists across take_frame()
        let pending_return = generator.has_pending_return();

        // Remember the stack depth before restoring the generator frame
        let initial_depth = ctx.stack_depth();

        // Restore the frame to context
        ctx.set_pending_realm_id(generator.realm_id);
        if let Err(e) = self.restore_generator_frame(ctx, &frame) {
            generator.complete();
            return GeneratorResult::Error(e);
        }

        // Handle pending throw (for generator.throw())
        if let Some(error) = pending_throw {
            // Inject throw - find try handler and jump to it, or error out
            if let Some((frame_depth, catch_pc)) = ctx.peek_nearest_try() {
                if frame_depth > initial_depth {
                    ctx.take_nearest_try(); // Actually pop it
                    while ctx.stack_depth() > frame_depth {
                        ctx.pop_frame_discard();
                    }
                    ctx.set_pc(catch_pc);
                    // Put error in register 0 for catch block (standard convention)
                    ctx.set_register(0, error.clone());
                    ctx.set_exception(error);
                } else {
                    generator.complete();
                    return GeneratorResult::Error(VmError::exception(error));
                }
            } else {
                generator.complete();
                return GeneratorResult::Error(VmError::exception(error));
            }
        } else if pending_return {
            // Handle pending return (for generator.return())
            // We need to run finally blocks. Trigger the exception path with a dummy value.
            let mut internal_handler = false;
            if let Some((frame_depth, catch_pc)) = ctx.peek_nearest_try() {
                if frame_depth > initial_depth {
                    internal_handler = true;
                    ctx.take_nearest_try(); // Actually pop it
                    while ctx.stack_depth() > frame_depth {
                        ctx.pop_frame_discard();
                    }
                    ctx.set_pc(catch_pc);
                    // Use pending return value as the exception object so it propagates
                    let pending_val = generator.get_pending_return().unwrap_or(Value::undefined());
                    ctx.set_exception(pending_val);
                }
            }

            if !internal_handler {
                // No internal try handlers, just return the value
                let return_value = generator
                    .take_pending_return()
                    .unwrap_or_else(Value::undefined);
                generator.complete();
                return GeneratorResult::Returned(return_value);
            }
        } else {
            // Normal resume - the sent value becomes the result of the yield expression
            // Store it in the destination register that was saved from the Yield instruction
            if let Some(dst) = yield_dst {
                ctx.set_register(dst, sent_value.unwrap_or_else(Value::undefined));
            }
        }

        // Run until yield or return (with panic protection)
        {
            use std::panic::{AssertUnwindSafe, catch_unwind};
            match catch_unwind(AssertUnwindSafe(|| {
                self.run_generator_loop(generator, ctx, initial_depth)
            })) {
                Ok(result) => result,
                Err(panic_payload) => {
                    generator.complete();
                    GeneratorResult::Error(VmError::internal(&panic_message(&panic_payload)))
                }
            }
        }
    }

    /// Restore a generator frame to the context
    fn restore_generator_frame(&self, ctx: &mut VmContext, frame: &GeneratorFrame) -> VmResult<()> {
        // Push a new frame with the saved state (no pending_args — we overwrite the window directly)
        ctx.set_pending_upvalues(frame.upvalues.clone());
        ctx.set_pending_this(frame.this_value.clone());

        // Get function info
        let func = frame
            .module
            .function(frame.function_index)
            .ok_or_else(|| VmError::internal("Generator function not found"))?;

        ctx.register_module(&frame.module);
        ctx.push_frame(
            frame.function_index,
            frame.module.module_id,
            func.local_count,
            None,
            frame.is_construct,
            false,
            frame.argc,
        )?;

        // Restore PC (push_frame sets it to 0, we need to set it to the saved value)
        ctx.set_pc(frame.pc);

        // Restore full register window (locals + scratch) in one pass
        ctx.restore_window(&frame.window);

        // Restore try stack
        for try_entry in &frame.try_stack {
            ctx.push_try(try_entry.catch_pc);
        }

        Ok(())
    }

    /// Save current execution state to a generator frame
    fn save_generator_frame(
        &self,
        ctx: &mut VmContext,
        _module: &Arc<Module>,
    ) -> VmResult<GeneratorFrame> {
        let current_frame = ctx
            .current_frame()
            .ok_or_else(|| VmError::internal("No current frame"))?;

        // Collect try stack entries for this frame
        let try_handlers = ctx.get_try_handlers_for_current_frame();
        let try_stack: Vec<crate::generator::TryEntry> = try_handlers
            .into_iter()
            .map(|(catch_pc, frame_depth)| crate::generator::TryEntry {
                catch_pc,
                frame_depth,
            })
            .collect();

        // Snapshot the full register window (locals + scratch) in one allocation
        let window = ctx.snapshot_window();
        let local_count = current_frame.local_count as usize;

        Ok(GeneratorFrame::new(
            current_frame.pc,
            current_frame.function_index,
            Arc::clone(ctx.module_table.get(current_frame.module_id)),
            local_count,
            window,
            current_frame.upvalues.clone(),
            try_stack,
            current_frame.this_value.clone(),
            current_frame.flags.is_construct(),
            current_frame.frame_id,
            current_frame.argc,
        ))
    }

    /// Reconstruct the try-stack by scanning instructions 0..target_pc for
    /// TryStart/TryEnd pairs. Used to recover exception handler state from
    /// JIT deopt buffers when building a GeneratorFrame.
    fn reconstruct_try_stack(
        func: &otter_vm_bytecode::Function,
        target_pc: usize,
        frame_depth: usize,
    ) -> Vec<crate::generator::TryEntry> {
        let instructions = func.instructions.read();
        let mut stack = Vec::new();
        for (pc, instruction) in instructions.iter().enumerate() {
            if pc >= target_pc {
                break;
            }
            match instruction {
                otter_vm_bytecode::Instruction::TryStart { catch_offset } => {
                    let catch_pc = (pc as isize + catch_offset.0 as isize) as usize;
                    stack.push(crate::generator::TryEntry {
                        catch_pc,
                        frame_depth,
                    });
                }
                otter_vm_bytecode::Instruction::TryEnd => {
                    stack.pop();
                }
                _ => {}
            }
        }
        stack
    }

    /// Attempt OSR into JIT code for a generator at a back-edge.
    ///
    /// Returns `Some(GeneratorResult)` if JIT handled the execution (either
    /// completed, yielded, or bailed out with state restored to the frame).
    /// Returns `None` to continue interpreting.
    fn try_generator_osr(
        &self,
        ctx: &mut VmContext,
        generator: &GcRef<JsGenerator>,
        module: &Arc<Module>,
        func: &otter_vm_bytecode::Function,
        target_pc: usize,
        initial_depth: usize,
    ) -> Option<GeneratorResult> {
        let newly_hot = func.record_back_edge_with_threshold(otter_vm_exec::jit_hot_threshold());
        if newly_hot {
            func.mark_hot();
            if otter_vm_exec::is_jit_enabled() {
                let func_index = ctx.current_frame()?.function_index;
                otter_vm_exec::enqueue_hot_function(module, func_index, func);
                otter_vm_exec::compile_one_pending_request(crate::jit_runtime::runtime_helpers());
                otter_vm_exec::record_back_edge_compilation();
            }
        }

        if !otter_vm_exec::is_jit_enabled()
            || !func.is_hot_function()
            || func.is_deoptimized()
            || func.jit_entry_ptr() == 0
            || func.flags.has_rest
            || func.flags.uses_arguments
            || func.flags.uses_eval
        {
            return None;
        }

        let frame = ctx.current_frame()?;
        if frame.flags.is_construct() || frame.flags.is_async() {
            return None;
        }
        let func_index = frame.function_index;
        let this_value = frame.this_value.clone();
        let home_object = frame.home_object.clone();
        let upvalues = frame.upvalues.clone();
        let argc = frame.argc;

        let local_count = func.local_count as usize;
        let reg_count = func.register_count as usize;
        let locals: Vec<Value> = (0..local_count)
            .map(|i| {
                ctx.get_local(i as u16)
                    .unwrap_or_else(|_| Value::undefined())
            })
            .collect();
        let registers: Vec<Value> = (0..reg_count)
            .map(|i| ctx.get_register(i as u16).clone())
            .collect();

        let param_count = func.param_count as usize;
        let arg_count = param_count.min(argc as usize);
        let args: Vec<Value> = (0..arg_count)
            .map(|i| {
                ctx.get_local(i as u16)
                    .unwrap_or_else(|_| Value::undefined())
            })
            .collect();

        ctx.set_pending_this(this_value.clone());
        if let Some(home_obj) = home_object {
            ctx.set_pending_home_object(home_obj);
        }

        otter_vm_exec::record_osr_attempt();

        let osr_state = crate::jit_runtime::OsrState {
            entry_pc: target_pc as u32,
            locals,
            registers,
        };

        let jit_interp: *const Self = self;
        let jit_ctx_ptr: *mut VmContext = ctx;
        match crate::jit_runtime::try_execute_jit(
            module.module_id,
            func_index,
            func,
            &args,
            ctx.cached_proto_epoch,
            jit_interp,
            jit_ctx_ptr,
            &module.constants as *const _,
            &upvalues,
            Some(osr_state),
        ) {
            crate::jit_runtime::JitCallResult::Ok(value) => {
                otter_vm_exec::record_osr_success();
                Some(GeneratorResult::Returned(value))
            }
            crate::jit_runtime::JitCallResult::BailoutResume(state) => {
                let try_stack =
                    Self::reconstruct_try_stack(func, state.bailout_pc as usize, initial_depth + 1);
                if let Some(resume) = crate::jit_resume::try_materialize_generator_yield(
                    ctx,
                    &state,
                    func,
                    func_index,
                    Arc::clone(module),
                    upvalues,
                    try_stack,
                    this_value.clone(),
                    generator.is_construct(),
                    argc,
                ) {
                    generator.suspend_with_frame(resume.frame);
                    ctx.pop_frame_discard();
                    return Some(GeneratorResult::Yielded(resume.yielded_value));
                }
                crate::jit_resume::resume_in_place(ctx, &state);
                None
            }
            crate::jit_runtime::JitCallResult::NeedsRecompilation => {
                otter_vm_exec::enqueue_hot_function(module, func_index, func);
                otter_vm_exec::compile_one_pending_request(crate::jit_runtime::runtime_helpers());
                None
            }
            crate::jit_runtime::JitCallResult::BailoutRestart
            | crate::jit_runtime::JitCallResult::NotCompiled => None,
        }
    }

    /// Run the generator execution loop until yield, return, or error
    ///
    /// `initial_depth` is the stack depth before the generator frame was pushed.
    /// This is used to correctly identify when the generator has returned.
    fn run_generator_loop(
        &self,
        generator: GcRef<JsGenerator>,
        ctx: &mut VmContext,
        initial_depth: usize,
    ) -> GeneratorResult {
        // Similar to run_loop but handles Yield specially
        let mut cached_module: Option<Arc<Module>> = None;
        let mut cached_frame_id: u32 = u32::MAX;

        // Hoist trace config: trace_state doesn't change mid-execution
        let tracing_enabled = ctx.trace_state.is_some();
        let trace_capture_timing = ctx
            .trace_state
            .as_ref()
            .map(|s| s.config.capture_timing)
            .unwrap_or(false);

        loop {
            // Periodic interrupt check
            if ctx.should_check_interrupt() {
                ctx.update_debug_snapshot();
                if ctx.is_interrupted() {
                    generator.complete();
                    return GeneratorResult::Error(VmError::interrupted());
                }
                ctx.maybe_collect_garbage();
                // Execute pending FinalizationRegistry cleanup callbacks
                if crate::weak_gc::has_pending_cleanups() {
                    let cleanups = crate::weak_gc::drain_pending_cleanups();
                    for (callback, held_value) in cleanups {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        let _ = ncx.call_function(&callback, Value::undefined(), &[held_value]);
                    }
                }
            }

            let frame = match ctx.current_frame() {
                Some(f) => f,
                None => {
                    // No more frames - generator completed with undefined
                    generator.complete();
                    return GeneratorResult::Returned(Value::undefined());
                }
            };

            // Cache module reference
            if frame.frame_id != cached_frame_id {
                cached_module = Some(Arc::clone(ctx.module_table.get(frame.module_id)));
                cached_frame_id = frame.frame_id;
            }

            let module_ref = cached_module.as_ref().unwrap();
            let func = match module_ref.function(frame.function_index) {
                Some(f) => f,
                None => {
                    generator.complete();
                    return GeneratorResult::Error(VmError::internal("Function not found"));
                }
            };

            // Check if we've reached the end
            if frame.pc >= func.instructions.read().len() {
                // Generator frame has no more instructions - pop it
                ctx.pop_frame_discard();

                // Check if we're back to the initial depth (generator is done)
                if ctx.stack_depth() <= initial_depth {
                    generator.complete();
                    // If we have a pending return from generator.return(), use it
                    if let Some(pending) = generator.take_pending_return() {
                        return GeneratorResult::Returned(pending);
                    }
                    return GeneratorResult::Returned(Value::undefined());
                }

                // There are still frames from nested calls - continue
                cached_frame_id = u32::MAX;
                continue;
            }

            let instruction = &func.instructions.read()[frame.pc];
            ctx.record_instruction();

            // Capture trace data (hoisted booleans avoid per-instruction Option probes)
            let trace_data = if tracing_enabled {
                Some((
                    frame.pc,
                    frame.function_index,
                    Arc::clone(ctx.module_table.get(frame.module_id)),
                    instruction.clone(),
                ))
            } else {
                None
            };
            let trace_start_time = if trace_capture_timing {
                Some(std::time::Instant::now())
            } else {
                None
            };

            // Execute instruction
            match self.execute_instruction(instruction, module_ref, ctx) {
                Ok(()) => {}
                Err(err) => match err {
                    VmError::TypeError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(ctx, "TypeError", &message)));
                    }
                    VmError::RangeError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(ctx, "RangeError", &message)));
                    }
                    VmError::ReferenceError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(ctx, "ReferenceError", &message)));
                    }
                    VmError::SyntaxError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(ctx, "SyntaxError", &message)));
                    }
                    VmError::URIError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(ctx, "URIError", &message)));
                    }
                    other => {
                        generator.complete();
                        return GeneratorResult::Error(other);
                    }
                },
            }

            // Record trace entry now that ctx is free (after execution, before state update)
            if let Some((pc, function_index, module, instruction)) = trace_data {
                let modified_registers = trace_modified_registers(&instruction, ctx);
                let execution_time_ns = trace_start_time
                    .map(|start| start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
                ctx.record_trace_entry(
                    &instruction,
                    pc,
                    function_index,
                    &module,
                    modified_registers,
                    execution_time_ns,
                );
            }

            if let Some(action) = ctx.take_dispatch_action() {
                match action {
                DispatchAction::Jump(offset) => {
                    if offset < 0 {
                        let target_pc = (ctx.current_frame().map(|f| f.pc).unwrap_or(0) as i64
                            + offset as i64) as usize;
                        let is_generator_frame = ctx.stack_depth() == initial_depth + 1;
                        if is_generator_frame {
                            if let Some(result) = self.try_generator_osr(
                                ctx,
                                &generator,
                                module_ref,
                                func,
                                target_pc,
                                initial_depth,
                            ) {
                                match result {
                                    GeneratorResult::Returned(v) => {
                                        generator.complete();
                                        while ctx.stack_depth() > initial_depth {
                                            ctx.pop_frame_discard();
                                        }
                                        return GeneratorResult::Returned(v);
                                    }
                                    GeneratorResult::Yielded(v) => {
                                        // Frame already popped by try_generator_osr
                                        return GeneratorResult::Yielded(v);
                                    }
                                    other => return other,
                                }
                            }
                        } else {
                            match self.try_back_edge_osr(ctx, module_ref, func, target_pc) {
                                BackEdgeOsrOutcome::Returned(osr_value) => {
                                    // Inner function call completed via OSR — treat as return
                                    let return_reg = ctx
                                        .current_frame()
                                        .map(|f| f.return_register)
                                        .unwrap_or(None);
                                    ctx.pop_frame_discard();
                                    if ctx.stack_depth() <= initial_depth {
                                        generator.complete();
                                        return GeneratorResult::Returned(osr_value);
                                    }
                                    if let Some(reg) = return_reg {
                                        ctx.set_register(reg, osr_value);
                                    }
                                    cached_frame_id = u32::MAX;
                                    continue;
                                }
                                BackEdgeOsrOutcome::ContinueAtDeoptPc => continue,
                                BackEdgeOsrOutcome::ContinueWithJump => {}
                            }
                        }
                    }
                    ctx.jump(offset);
                }
                DispatchAction::Return(value) => {
                    // Pop the current frame
                    let frame = ctx.pop_frame().unwrap();

                    // Check if we're back to the initial depth (generator is returning)
                    if ctx.stack_depth() <= initial_depth {
                        generator.complete();
                        // If we have a pending return from generator.return(), use it
                        if let Some(pending) = generator.take_pending_return() {
                            return GeneratorResult::Returned(pending);
                        }
                        return GeneratorResult::Returned(value);
                    }

                    // There's a caller frame within the generator - pass return value
                    if let Some(ret_reg) = frame.return_register {
                        ctx.set_register(ret_reg, value);
                    }
                    cached_frame_id = u32::MAX;
                }
                DispatchAction::Yield { value, yield_dst } => {
                    // Save frame state before advancing PC
                    ctx.advance_pc(); // Move past the yield instruction

                    match self.save_generator_frame(ctx, module_ref) {
                        Ok(mut frame) => {
                            // Store the yield destination register so we know where
                            // to put the sent value when resuming
                            frame.yield_dst = Some(yield_dst);
                            generator.suspend_with_frame(frame);
                        }
                        Err(e) => {
                            generator.complete();
                            return GeneratorResult::Error(e);
                        }
                    }

                    // Pop the generator's frame from context
                    ctx.pop_frame_discard();

                    return GeneratorResult::Yielded(value);
                }
                DispatchAction::Throw(error) => {
                    // Try to find a catch handler inside the generator
                    if let Some((frame_depth, catch_pc)) = ctx.peek_nearest_try() {
                        if frame_depth > initial_depth {
                            ctx.take_nearest_try(); // Actually pop it
                            // Unwind to the handler frame
                            while ctx.stack_depth() > frame_depth {
                                ctx.pop_frame_discard();
                            }
                            ctx.set_pc(catch_pc);
                            ctx.set_exception(error.clone());
                            // Put error in register 0 for catch block
                            ctx.set_register(0, error);
                            cached_frame_id = u32::MAX;
                            continue;
                        }
                    }

                    // No internal handler - check pending return from generator.return()
                    if let Some(return_value) = generator.take_pending_return() {
                        generator.complete();
                        while ctx.stack_depth() > initial_depth {
                            ctx.pop_frame_discard();
                        }
                        return GeneratorResult::Returned(return_value);
                    }

                    // No internal handler - completion will buble out
                    generator.complete();
                    // Pop all frames down to initial_depth
                    while ctx.stack_depth() > initial_depth {
                        ctx.pop_frame_discard();
                    }
                    return GeneratorResult::Error(VmError::exception(error));
                }
                DispatchAction::Call {
                    func_index,
                    module_id,
                    argc,
                    return_reg,
                    is_construct,
                    is_async,
                    upvalues,
                } => {
                    ctx.advance_pc();
                    // Extract func info with scoped borrow (no Arc clone)
                    let (local_count, has_rest, param_count) = {
                        let m = ctx.module_table.get(module_id);
                        match m.function(func_index) {
                            Some(f) => (f.local_count, f.flags.has_rest, f.param_count as usize),
                            None => {
                                generator.complete();
                                return GeneratorResult::Error(VmError::internal(format!(
                                    "callee not found (func_index={}, function_count={})",
                                    func_index,
                                    m.function_count()
                                )));
                            }
                        }
                    };

                    if has_rest {
                        let mut args = ctx.take_pending_args();
                        let rest_args: Vec<Value> = if args.len() > param_count {
                            args.drain(param_count..).collect()
                        } else {
                            Vec::new()
                        };

                        let rest_arr = GcRef::new(JsObject::array(rest_args.len()));
                        if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object())
                        {
                            rest_arr.set_prototype(Value::object(array_proto));
                        }
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            let _ = rest_arr.set(PropertyKey::Index(i as u32), arg);
                        }

                        args.push(Value::object(rest_arr));
                        ctx.set_pending_args(args);
                    }

                    ctx.set_pending_upvalues(upvalues);

                    if let Err(e) = ctx.push_frame(
                        func_index,
                        module_id,
                        local_count,
                        Some(return_reg),
                        is_construct,
                        is_async,
                        argc as u16,
                    ) {
                        generator.complete();
                        return GeneratorResult::Error(e);
                    }
                }
                DispatchAction::TailCall {
                    func_index,
                    module_id,
                    argc,
                    return_reg,
                    is_async,
                    upvalues,
                } => {
                    ctx.pop_frame_discard();
                    cached_frame_id = u32::MAX;

                    let (local_count, has_rest, param_count) = {
                        let m = ctx.module_table.get(module_id);
                        match m.function(func_index) {
                            Some(f) => (f.local_count, f.flags.has_rest, f.param_count as usize),
                            None => {
                                generator.complete();
                                return GeneratorResult::Error(VmError::internal(format!(
                                    "callee not found (func_index={}, function_count={})",
                                    func_index,
                                    m.function_count()
                                )));
                            }
                        }
                    };

                    if has_rest {
                        let mut args = ctx.take_pending_args();
                        let rest_args: Vec<Value> = if args.len() > param_count {
                            args.drain(param_count..).collect()
                        } else {
                            Vec::new()
                        };
                        let rest_arr = GcRef::new(JsObject::array(rest_args.len()));
                        if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object())
                        {
                            rest_arr.set_prototype(Value::object(array_proto));
                        }
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            let _ = rest_arr.set(PropertyKey::Index(i as u32), arg);
                        }
                        args.push(Value::object(rest_arr));
                        ctx.set_pending_args(args);
                    }

                    ctx.set_pending_upvalues(upvalues);

                    if let Err(e) = ctx.push_frame(
                        func_index,
                        module_id,
                        local_count,
                        Some(return_reg),
                        false,
                        is_async,
                        argc as u16,
                    ) {
                        generator.complete();
                        return GeneratorResult::Error(e);
                    }
                }
                DispatchAction::Suspend {
                    promise,
                    resume_reg,
                } => {
                    // Await in async generator - suspend and return the promise
                    if generator.is_async() {
                        // Save frame state before advancing PC
                        ctx.advance_pc(); // Move past the await instruction

                        match self.save_generator_frame(ctx, module_ref) {
                            Ok(mut frame) => {
                                // Store the await resume register so we know where
                                // to put the resolved value when resuming
                                frame.yield_dst = Some(resume_reg);
                                generator.suspend_with_frame(frame);
                            }
                            Err(e) => {
                                generator.complete();
                                return GeneratorResult::Error(e);
                            }
                        }

                        // Pop the generator's frame from context
                        ctx.pop_frame_discard();

                        return GeneratorResult::Suspended {
                            promise,
                            resume_reg,
                            generator,
                        };
                    } else {
                        // Sync generators cannot await
                        generator.complete();
                        return GeneratorResult::Error(VmError::internal(
                            "Sync generator cannot use await",
                        ));
                    }
                }
                } // match action
            } else {
                ctx.advance_pc();
            }
        }
    }
}
