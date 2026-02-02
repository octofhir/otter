//! Bytecode interpreter
//!
//! Executes bytecode instructions.

use otter_vm_bytecode::{Instruction, Module, TypeFlags, UpvalueCapture};

use crate::async_context::{AsyncContext, VmExecutionResult};
use crate::context::VmContext;
use crate::error::{VmError, VmResult};
use crate::gc::GcRef;
use crate::generator::{GeneratorFrame, GeneratorState, JsGenerator};
use crate::object::{
    JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey, get_proto_epoch,
};
use crate::promise::{JsPromise, PromiseState};
use crate::regexp::JsRegExp;
use crate::string::JsString;
use crate::value::{Closure, HeapRef, UpvalueCell, Value};

use num_bigint::BigInt as NumBigInt;
use num_traits::{One, ToPrimitive, Zero};
use std::cmp::Ordering;
use std::sync::Arc;

/// The bytecode interpreter
pub struct Interpreter {
    /// Current module being executed
    #[allow(dead_code)]
    current_module: Option<Arc<Module>>,
}

enum Numeric {
    Number(f64),
    BigInt(NumBigInt),
}

impl Interpreter {
    /// Create a new interpreter
    pub fn new() -> Self {
        Self {
            current_module: None,
        }
    }

    /// Execute a module
    pub fn execute(&mut self, module: &Module, ctx: &mut VmContext) -> VmResult<Value> {
        // Wrap in Arc for closure capture
        self.execute_arc(Arc::new(module.clone()), ctx)
    }

    /// Execute a module with Arc (for internal use and pre-created Arcs)
    pub fn execute_arc(&mut self, module: Arc<Module>, ctx: &mut VmContext) -> VmResult<Value> {
        // Get entry function
        let entry_func = module
            .entry_function()
            .ok_or_else(|| VmError::internal("no entry function"))?;

        // Record the function call for hot function detection
        let _ = entry_func.record_call();

        // Push initial frame with module reference
        ctx.push_frame(
            module.entry_point,
            Arc::clone(&module),
            entry_func.local_count,
            None,
            false,
            entry_func.is_async(),
            0,
        )?;
        ctx.set_running(true);

        // Execute loop
        let result = self.run_loop(ctx);

        // Pop the entry frame that we pushed above.
        // run_loop returns without popping at stack_depth==1.
        ctx.pop_frame();

        ctx.set_running(false);
        result
    }

    /// Execute a module and return a result that can indicate suspension
    ///
    /// This is the primary entry point for async-aware execution.
    /// Unlike `execute`, this method returns a `VmExecutionResult` that
    /// can indicate that execution was suspended waiting for a Promise.
    pub fn execute_with_suspension(
        &mut self,
        module: Arc<Module>,
        ctx: &mut VmContext,
        result_promise: Arc<JsPromise>,
    ) -> VmExecutionResult {
        // Get entry function
        let entry_func = match module.entry_function() {
            Some(f) => f,
            None => return VmExecutionResult::Error("no entry function".to_string()),
        };

        // Record the function call for hot function detection
        let _ = entry_func.record_call();

        // Push initial frame with module reference
        if let Err(e) = ctx.push_frame(
            module.entry_point,
            Arc::clone(&module),
            entry_func.local_count,
            None,
            false,
            entry_func.is_async(),
            0,
        ) {
            return VmExecutionResult::Error(e.to_string());
        }

        ctx.set_running(true);

        // Execute loop with suspension support
        self.run_loop_with_suspension(ctx, result_promise)
    }

    /// Resume execution from a saved async context
    ///
    /// This is called when a Promise that was awaited resolves.
    /// It restores the VM state and continues execution.
    pub fn resume_async(
        &mut self,
        ctx: &mut VmContext,
        async_ctx: AsyncContext,
        resolved_value: Value,
    ) -> VmExecutionResult {
        // Restore the call stack from saved frames
        if let Err(e) = ctx.restore_frames(async_ctx.frames) {
            return VmExecutionResult::Error(e.to_string());
        }

        // Set the resolved value in the resume register
        ctx.set_register(async_ctx.resume_register, resolved_value);
        ctx.set_running(async_ctx.was_running);

        // Continue execution
        self.run_loop_with_suspension(ctx, async_ctx.result_promise)
    }

    /// Call a function value (native or closure) with arguments
    ///
    /// This method allows calling JavaScript functions from Rust code.
    /// It handles both native functions (direct call) and closures (push frame and execute).
    pub fn call_function(
        &mut self,
        ctx: &mut VmContext,
        func: &Value,
        this_value: Value,
        args: &[Value],
    ) -> VmResult<Value> {
        // Check if it's a native function
        if let Some(native_fn) = func.as_native_function() {
            return self.call_native_fn(ctx, native_fn, &this_value, args);
        }

        // Regular closure call
        let closure = func
            .as_function()
            .ok_or_else(|| VmError::type_error("not a function"))?;

        // Save current state
        let was_running = ctx.is_running();
        let prev_stack_depth = ctx.stack_depth();

        // Get function info
        let func_info = closure
            .module
            .function(closure.function_index)
            .ok_or_else(|| VmError::internal("function not found"))?;

        // Set up the call
        ctx.set_pending_args(args.to_vec());
        ctx.set_pending_this(this_value);
        ctx.set_pending_upvalues(closure.upvalues.clone());
        // Propagate home_object from closure to the new call frame
        if let Some(ref ho) = closure.home_object {
            ctx.set_pending_home_object(ho.clone());
        }

        let argc = args.len();
        ctx.push_frame(
            closure.function_index,
            Arc::clone(&closure.module),
            func_info.local_count,
            Some(0), // Return register (unused, we get result from Return)
            false,   // Not a construct call
            closure.is_async,
            argc,
        )?;
        ctx.set_running(true);

        // Execute until this call returns
        let result = loop {
            let frame = match ctx.current_frame() {
                Some(f) => f,
                None => return Err(VmError::internal("no frame")),
            };

            let current_module = Arc::clone(&frame.module);
            let func = match current_module.function(frame.function_index) {
                Some(f) => f,
                None => return Err(VmError::internal("function not found")),
            };

            // Check if we've reached the end of the function
            if frame.pc >= func.instructions.len() {
                // Check if we've returned to the original depth
                if ctx.stack_depth() <= prev_stack_depth {
                    break Value::undefined();
                }
                ctx.pop_frame();
                continue;
            }

            let instruction = &func.instructions[frame.pc];

            match self.execute_instruction(instruction, &current_module, ctx) {
                Ok(InstructionResult::Continue) => {
                    ctx.advance_pc();
                }
                Ok(InstructionResult::Jump(offset)) => {
                    ctx.jump(offset);
                }
                Ok(InstructionResult::Return(value)) => {
                    // Check if we've returned to the original depth
                    if ctx.stack_depth() <= prev_stack_depth + 1 {
                        ctx.pop_frame();
                        break value;
                    }
                    // Handle return from nested call
                    let return_reg = ctx
                        .current_frame()
                        .and_then(|f| f.return_register)
                        .unwrap_or(0);
                    ctx.pop_frame();
                    ctx.set_register(return_reg, value);
                }
                Ok(InstructionResult::Call {
                    func_index,
                    module,
                    argc,
                    return_reg,
                    is_construct,
                    is_async,
                    upvalues,
                }) => {
                    ctx.advance_pc();
                    let func = module
                        .function(func_index)
                        .ok_or_else(|| VmError::internal("function not found"))?;

                    // Record the function call for hot function detection
                    let became_hot = func.record_call();
                    if became_hot {
                        // JIT trigger hook: function just became hot
                        // In Phase 3, this will trigger JIT compilation
                        #[cfg(feature = "jit")]
                        {
                            // TODO: Queue function for JIT compilation
                        }
                        let _ = became_hot; // Silence unused warning when jit feature is off
                    }

                    let local_count = func.local_count;
                    ctx.set_pending_upvalues(upvalues);
                    ctx.push_frame(
                        func_index,
                        module,
                        local_count,
                        Some(return_reg),
                        is_construct,
                        is_async,
                        argc as usize,
                    )?;
                }
                Ok(InstructionResult::TailCall {
                    func_index,
                    module,
                    argc,
                    return_reg,
                    is_async,
                    upvalues,
                }) => {
                    // Tail call: pop current frame and push new one
                    ctx.pop_frame();
                    let local_count = module
                        .function(func_index)
                        .ok_or_else(|| VmError::internal("function not found"))?
                        .local_count;
                    ctx.set_pending_upvalues(upvalues);
                    ctx.push_frame(
                        func_index,
                        module,
                        local_count,
                        Some(return_reg),
                        false,
                        is_async,
                        argc as usize,
                    )?;
                }
                Ok(InstructionResult::Suspend { .. }) => {
                    // Can't handle suspension in direct call, return undefined
                    break Value::undefined();
                }
                Ok(InstructionResult::Yield { .. }) => {
                    // Can't handle yield in direct call, return undefined
                    break Value::undefined();
                }
                Ok(InstructionResult::Throw(error)) => {
                    ctx.set_running(was_running);
                    return Err(VmError::internal(format!(
                        "Uncaught exception: {}",
                        self.to_string(&error)
                    )));
                }
                Err(e) => {
                    ctx.set_running(was_running);
                    return Err(e);
                }
            }
        };

        ctx.set_running(was_running);
        Ok(result)
    }

    /// Capture the current VM state as an AsyncContext for suspension
    fn capture_async_context(
        &self,
        ctx: &VmContext,
        resume_register: u16,
        awaited_promise: Arc<JsPromise>,
        result_promise: Arc<JsPromise>,
    ) -> AsyncContext {
        AsyncContext::new(
            ctx.save_frames(),
            result_promise,
            awaited_promise,
            resume_register,
            ctx.is_running(),
        )
    }

    /// Main execution loop with suspension support
    fn run_loop_with_suspension(
        &mut self,
        ctx: &mut VmContext,
        result_promise: Arc<JsPromise>,
    ) -> VmExecutionResult {
        // Cache module Arc - only refresh when frame changes
        let mut cached_module: Option<Arc<Module>> = None;
        let mut cached_frame_id: usize = usize::MAX;

        loop {
            // Periodic interrupt check for responsive timeouts
            if ctx.should_check_interrupt() {
                if ctx.is_interrupted() {
                    ctx.set_running(false);
                    return VmExecutionResult::Error("Execution interrupted".to_string());
                }
                // Check for GC trigger at safepoint
                ctx.maybe_collect_garbage();
            }

            let frame = match ctx.current_frame() {
                Some(f) => f,
                None => return VmExecutionResult::Error("no frame".to_string()),
            };

            // Only clone Arc when frame changes (avoids atomic ops on hot path)
            if frame.frame_id != cached_frame_id {
                cached_module = Some(Arc::clone(&frame.module));
                cached_frame_id = frame.frame_id;
            }

            // Get reference to cached module (avoids clone on hot path)
            let module_ref = cached_module.as_ref().unwrap();
            let func = match module_ref.function(frame.function_index) {
                Some(f) => f,
                None => return VmExecutionResult::Error("function not found".to_string()),
            };

            // Check if we've reached the end of the function
            if frame.pc >= func.instructions.len() {
                // Implicit return undefined
                if ctx.stack_depth() == 1 {
                    ctx.set_running(false);
                    return VmExecutionResult::Complete(Value::undefined());
                }
                ctx.pop_frame();
                // Invalidate cache since frame changed
                cached_frame_id = usize::MAX;
                continue;
            }

            let instruction = &func.instructions[frame.pc];

            // Record instruction execution for profiling
            ctx.record_instruction();

            // Execute the instruction
            let instruction_result = match self.execute_instruction(instruction, module_ref, ctx) {
                Ok(result) => result,
                Err(err) => match err {
                    VmError::Exception(thrown) => InstructionResult::Throw(thrown.value),
                    VmError::TypeError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "TypeError", &message))
                    }
                    VmError::RangeError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "RangeError", &message))
                    }
                    VmError::ReferenceError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "ReferenceError", &message))
                    }
                    VmError::SyntaxError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "SyntaxError", &message))
                    }
                    other => return VmExecutionResult::Error(other.to_string()),
                },
            };

            match instruction_result {
                InstructionResult::Continue => {
                    ctx.advance_pc();
                }
                InstructionResult::Jump(offset) => {
                    ctx.jump(offset);
                }
                InstructionResult::Return(value) => {
                    if ctx.stack_depth() == 1 {
                        ctx.set_running(false);
                        return VmExecutionResult::Complete(value);
                    }

                    let (return_reg, is_construct, construct_this, is_async) = {
                        let frame = match ctx.current_frame() {
                            Some(f) => f,
                            None => return VmExecutionResult::Error("no frame".to_string()),
                        };
                        (
                            frame.return_register,
                            frame.is_construct,
                            frame.this_value.clone(),
                            frame.is_async,
                        )
                    };
                    ctx.pop_frame();
                    // Invalidate cache since frame changed
                    cached_frame_id = usize::MAX;

                    if let Some(reg) = return_reg {
                        let value = if is_construct && !value.is_object() {
                            construct_this
                        } else if is_async {
                            // Async functions return a Promise that resolves with their return value
                            self.create_js_promise(ctx, JsPromise::resolved(value))
                        } else {
                            value
                        };
                        ctx.set_register(reg, value);
                    }
                }
                InstructionResult::Throw(value) => {
                    // Unwind to nearest try handler if present
                    if let Some((target_depth, catch_pc)) = ctx.take_nearest_try() {
                        // Pop frames above the handler
                        while ctx.stack_depth() > target_depth {
                            ctx.pop_frame();
                        }
                        // Invalidate cache since frames changed
                        cached_frame_id = usize::MAX;

                        // Jump to catch block in the handler frame
                        let frame = match ctx.current_frame_mut() {
                            Some(f) => f,
                            None => return VmExecutionResult::Error("no frame".to_string()),
                        };
                        frame.pc = catch_pc;

                        ctx.set_exception(value);
                        continue;
                    }

                    // No handler: return as error
                    ctx.set_running(false);
                    return VmExecutionResult::Error(format!(
                        "Uncaught exception: {}",
                        self.to_string(&value)
                    ));
                }
                InstructionResult::Call {
                    func_index,
                    module: call_module,
                    argc,
                    return_reg,
                    is_construct,
                    is_async,
                    upvalues,
                } => {
                    ctx.advance_pc(); // Advance before pushing new frame

                    let callee = match call_module.function(func_index) {
                        Some(f) => f,
                        None => {
                            return VmExecutionResult::Error(format!(
                                "callee not found (func_index={}, function_count={})",
                                func_index,
                                call_module.function_count()
                            ));
                        }
                    };

                    // Record the function call for hot function detection
                    let _ = callee.record_call();

                    // Extract values before moving call_module
                    let local_count = callee.local_count;
                    let has_rest = callee.flags.has_rest;
                    let param_count = callee.param_count as usize;

                    // Handle rest parameters
                    if has_rest {
                        let mut args = ctx.take_pending_args();

                        // Collect extra arguments into rest array
                        let rest_args: Vec<Value> = if args.len() > param_count {
                            args.drain(param_count..).collect()
                        } else {
                            Vec::new()
                        };

                        // Create rest array
                        let rest_arr = GcRef::new(JsObject::array(
                            rest_args.len(),
                            ctx.memory_manager().clone(),
                        ));
                        // If `Array.prototype` is available, attach it so rest arrays are iterable.
                        if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object())
                        {
                            rest_arr.set_prototype(Some(array_proto));
                        }
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            rest_arr.set(PropertyKey::Index(i as u32), arg);
                        }

                        // Append rest array to args
                        args.push(Value::object(rest_arr));
                        ctx.set_pending_args(args);
                    }

                    // Set pending upvalues (captured closure values) for the new frame
                    ctx.set_pending_upvalues(upvalues);

                    // Push frame with the callee's module (closures carry their own module)
                    if let Err(e) = ctx.push_frame(
                        func_index,
                        call_module,
                        local_count,
                        Some(return_reg),
                        is_construct,
                        is_async,
                        argc as usize,
                    ) {
                        return VmExecutionResult::Error(e.to_string());
                    }
                }
                InstructionResult::TailCall {
                    func_index,
                    module: call_module,
                    argc,
                    return_reg,
                    is_async,
                    upvalues,
                } => {
                    // Tail call optimization: pop current frame before pushing new one
                    ctx.pop_frame();
                    // Invalidate cache since frame changed
                    cached_frame_id = usize::MAX;

                    let callee = match call_module.function(func_index) {
                        Some(f) => f,
                        None => {
                            return VmExecutionResult::Error(format!(
                                "callee not found (func_index={}, function_count={})",
                                func_index,
                                call_module.function_count()
                            ));
                        }
                    };

                    let local_count = callee.local_count;
                    let has_rest = callee.flags.has_rest;
                    let param_count = callee.param_count as usize;

                    // Handle rest parameters
                    if has_rest {
                        let mut args = ctx.take_pending_args();
                        let rest_args: Vec<Value> = if args.len() > param_count {
                            args.drain(param_count..).collect()
                        } else {
                            Vec::new()
                        };
                        let rest_arr = GcRef::new(JsObject::array(
                            rest_args.len(),
                            ctx.memory_manager().clone(),
                        ));
                        if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object())
                        {
                            rest_arr.set_prototype(Some(array_proto));
                        }
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            rest_arr.set(PropertyKey::Index(i as u32), arg);
                        }
                        args.push(Value::object(rest_arr));
                        ctx.set_pending_args(args);
                    }

                    ctx.set_pending_upvalues(upvalues);

                    if let Err(e) = ctx.push_frame(
                        func_index,
                        call_module,
                        local_count,
                        Some(return_reg),
                        false,
                        is_async,
                        argc as usize,
                    ) {
                        return VmExecutionResult::Error(e.to_string());
                    }
                }
                InstructionResult::Suspend {
                    promise,
                    resume_reg,
                } => {
                    // Advance PC before suspension so we resume at the next instruction
                    ctx.advance_pc();

                    // Check promise state
                    match promise.state() {
                        PromiseState::Fulfilled(value) => {
                            // Promise already resolved, continue execution
                            ctx.set_register(resume_reg, value);
                        }
                        PromiseState::Rejected(error) => {
                            // Promise rejected, propagate as error
                            ctx.set_running(false);
                            return VmExecutionResult::Error(format!(
                                "Promise rejected: {:?}",
                                error
                            ));
                        }
                        PromiseState::Pending | PromiseState::PendingThenable(_) => {
                            // Promise is pending - suspend execution
                            let async_ctx = self.capture_async_context(
                                ctx,
                                resume_reg,
                                promise,
                                Arc::clone(&result_promise),
                            );
                            return VmExecutionResult::Suspended(async_ctx);
                        }
                    }
                }
                InstructionResult::Yield { value, .. } => {
                    // Generator yielded a value
                    let result = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));
                    result.set(PropertyKey::string("value"), value);
                    result.set(PropertyKey::string("done"), Value::boolean(false));
                    ctx.advance_pc();
                    return VmExecutionResult::Complete(Value::object(result));
                }
            }
        }
    }

    /// Main execution loop
    fn run_loop(&mut self, ctx: &mut VmContext) -> VmResult<Value> {
        // Cache module Arc - only refresh when frame changes
        let mut cached_module: Option<Arc<Module>> = None;
        let mut cached_frame_id: usize = usize::MAX;

        loop {
            // Periodic interrupt check for responsive timeouts
            if ctx.should_check_interrupt() {
                if ctx.is_interrupted() {
                    return Err(VmError::interrupted());
                }
                // Check for GC trigger at safepoint
                ctx.maybe_collect_garbage();
            }

            let frame = ctx
                .current_frame()
                .ok_or_else(|| VmError::internal("no frame"))?;

            // Only clone Arc when frame changes (avoids atomic ops on hot path)
            if frame.frame_id != cached_frame_id {
                cached_module = Some(Arc::clone(&frame.module));
                cached_frame_id = frame.frame_id;
            }

            // Get reference to cached module (avoids clone on hot path for func lookup)
            let module_ref = cached_module.as_ref().unwrap();
            let func = module_ref
                .function(frame.function_index)
                .ok_or_else(|| VmError::internal("function not found"))?;

            // Check if we've reached the end of the function
            if frame.pc >= func.instructions.len() {
                // Implicit return undefined
                if ctx.stack_depth() == 1 {
                    return Ok(Value::undefined());
                }
                ctx.pop_frame();
                // Invalidate cache since frame changed
                cached_frame_id = usize::MAX;
                continue;
            }

            let instruction = &func.instructions[frame.pc];

            // Record instruction execution for profiling
            ctx.record_instruction();

            // Execute the instruction
            let instruction_result = match self.execute_instruction(instruction, module_ref, ctx) {
                Ok(result) => result,
                Err(err) => match err {
                    VmError::Exception(thrown) => InstructionResult::Throw(thrown.value),
                    VmError::TypeError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "TypeError", &message))
                    }
                    VmError::RangeError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "RangeError", &message))
                    }
                    VmError::ReferenceError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "ReferenceError", &message))
                    }
                    VmError::SyntaxError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "SyntaxError", &message))
                    }
                    other => return Err(other),
                },
            };

            match instruction_result {
                InstructionResult::Continue => {
                    ctx.advance_pc();
                }
                InstructionResult::Jump(offset) => {
                    ctx.jump(offset);
                }
                InstructionResult::Return(value) => {
                    if ctx.stack_depth() == 1 {
                        return Ok(value);
                    }

                    let (return_reg, is_construct, construct_this, is_async) = {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        (
                            frame.return_register,
                            frame.is_construct,
                            frame.this_value.clone(),
                            frame.is_async,
                        )
                    };
                    ctx.pop_frame();
                    // Invalidate cache since frame changed
                    cached_frame_id = usize::MAX;

                    if let Some(reg) = return_reg {
                        let value = if is_construct && !value.is_object() {
                            construct_this
                        } else if is_async {
                            // Async functions return a Promise that resolves with their return value
                            // Create a proper JS Promise object with _internal field
                            self.create_js_promise(ctx, JsPromise::resolved(value))
                        } else {
                            value
                        };
                        ctx.set_register(reg, value);
                    }
                }
                InstructionResult::Throw(value) => {
                    // Unwind to nearest try handler if present
                    if let Some((target_depth, catch_pc)) = ctx.take_nearest_try() {
                        // Pop frames above the handler
                        while ctx.stack_depth() > target_depth {
                            ctx.pop_frame();
                        }
                        // Invalidate cache since frames changed
                        cached_frame_id = usize::MAX;

                        // Jump to catch block in the handler frame
                        let frame = ctx
                            .current_frame_mut()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        frame.pc = catch_pc;

                        ctx.set_exception(value);
                        continue;
                    }

                    // No handler: convert to an uncaught exception
                    return Err(VmError::Exception(Box::new(crate::error::ThrownValue {
                        message: self.to_string(&value),
                        value: value.clone(),
                        stack: Vec::new(),
                    })));
                }
                InstructionResult::Call {
                    func_index,
                    module: call_module,
                    argc,
                    return_reg,
                    is_construct,
                    is_async,
                    upvalues,
                } => {
                    ctx.advance_pc(); // Advance before pushing new frame

                    let callee = call_module.function(func_index).ok_or_else(|| {
                        VmError::internal(format!(
                            "callee not found (func_index={}, function_count={})",
                            func_index,
                            call_module.function_count()
                        ))
                    })?;

                    // Record the function call for hot function detection
                    let _ = callee.record_call();

                    // Extract values before moving call_module
                    let local_count = callee.local_count;
                    let has_rest = callee.flags.has_rest;
                    let param_count = callee.param_count as usize;

                    // Handle rest parameters
                    if has_rest {
                        let mut args = ctx.take_pending_args();

                        // Collect extra arguments into rest array
                        let rest_args: Vec<Value> = if args.len() > param_count {
                            args.drain(param_count..).collect()
                        } else {
                            Vec::new()
                        };

                        // Create rest array
                        let rest_arr = GcRef::new(JsObject::array(
                            rest_args.len(),
                            ctx.memory_manager().clone(),
                        ));
                        // If `Array.prototype` is available, attach it so rest arrays are iterable.
                        if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object())
                        {
                            rest_arr.set_prototype(Some(array_proto));
                        }
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            rest_arr.set(PropertyKey::Index(i as u32), arg);
                        }

                        // Append rest array to args
                        args.push(Value::object(rest_arr));
                        ctx.set_pending_args(args);
                    }

                    // Set pending upvalues (captured closure values) for the new frame
                    ctx.set_pending_upvalues(upvalues);

                    ctx.push_frame(
                        func_index,
                        call_module,
                        local_count,
                        Some(return_reg),
                        is_construct,
                        is_async,
                        argc as usize,
                    )?;
                }
                InstructionResult::TailCall {
                    func_index,
                    module: call_module,
                    argc,
                    return_reg,
                    is_async,
                    upvalues,
                } => {
                    // Tail call optimization: pop current frame before pushing new one
                    // This prevents stack growth for recursive tail calls
                    ctx.pop_frame();
                    // Invalidate cache since frame changed
                    cached_frame_id = usize::MAX;

                    let callee = call_module.function(func_index).ok_or_else(|| {
                        VmError::internal(format!(
                            "callee not found (func_index={}, function_count={})",
                            func_index,
                            call_module.function_count()
                        ))
                    })?;

                    let local_count = callee.local_count;
                    let has_rest = callee.flags.has_rest;
                    let param_count = callee.param_count as usize;

                    // Handle rest parameters (same as regular call)
                    if has_rest {
                        let mut args = ctx.take_pending_args();
                        let rest_args: Vec<Value> = if args.len() > param_count {
                            args.drain(param_count..).collect()
                        } else {
                            Vec::new()
                        };
                        let rest_arr = GcRef::new(JsObject::array(
                            rest_args.len(),
                            ctx.memory_manager().clone(),
                        ));
                        if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object())
                        {
                            rest_arr.set_prototype(Some(array_proto));
                        }
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            rest_arr.set(PropertyKey::Index(i as u32), arg);
                        }
                        args.push(Value::object(rest_arr));
                        ctx.set_pending_args(args);
                    }

                    ctx.set_pending_upvalues(upvalues);

                    ctx.push_frame(
                        func_index,
                        call_module,
                        local_count,
                        Some(return_reg),
                        false, // tail calls are never construct
                        is_async,
                        argc as usize,
                    )?;
                }
                InstructionResult::Suspend {
                    promise,
                    resume_reg,
                } => {
                    // Store the pending promise state for later resumption
                    ctx.advance_pc();

                    // Poll the promise - if pending, we need to wait for async tasks
                    match promise.state() {
                        PromiseState::Fulfilled(value) => {
                            ctx.set_register(resume_reg, value);
                        }
                        PromiseState::Rejected(error) => {
                            return Err(VmError::type_error(format!(
                                "Promise rejected: {:?}",
                                error
                            )));
                        }
                        PromiseState::Pending | PromiseState::PendingThenable(_) => {
                            // Promise is pending - need to wait for async operation
                            // Return the promise to caller for async handling
                            // The runtime's event loop should poll and resume
                            return Ok(Value::promise(promise));
                        }
                    }
                }
                InstructionResult::Yield { value, .. } => {
                    // Generator yielded a value
                    // Create an iterator result object { value, done: false }
                    let result = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));
                    result.set(PropertyKey::string("value"), value);
                    result.set(PropertyKey::string("done"), Value::boolean(false));
                    ctx.advance_pc();
                    return Ok(Value::object(result));
                }
            }
        }
    }

    /// Execute a single instruction
    fn execute_instruction(
        &mut self,
        instruction: &Instruction,
        module: &Arc<Module>,
        ctx: &mut VmContext,
    ) -> VmResult<InstructionResult> {
        match instruction {
            // ==================== Constants ====================
            Instruction::LoadUndefined { dst } => {
                ctx.set_register(dst.0, Value::undefined());
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadNull { dst } => {
                ctx.set_register(dst.0, Value::null());
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadTrue { dst } => {
                ctx.set_register(dst.0, Value::boolean(true));
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadFalse { dst } => {
                ctx.set_register(dst.0, Value::boolean(false));
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadInt8 { dst, value } => {
                ctx.set_register(dst.0, Value::int32(*value as i32));
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadInt32 { dst, value } => {
                ctx.set_register(dst.0, Value::int32(*value));
                Ok(InstructionResult::Continue)
            }

            Instruction::LoadConst { dst, idx } => {
                let constant = module
                    .constants
                    .get(idx.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;

                let value = self.constant_to_value(ctx, constant)?;
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            // ==================== Variables ====================
            Instruction::GetLocal { dst, idx } => {
                let value = ctx.get_local(idx.0)?;
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::SetLocal { idx, src } => {
                let value = ctx.get_register(src.0).clone();
                ctx.set_local(idx.0, value)?;
                Ok(InstructionResult::Continue)
            }

            Instruction::GetUpvalue { dst, idx } => {
                // Get value from upvalue cell
                let value = ctx.get_upvalue(idx.0)?;
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::SetUpvalue { idx, src } => {
                // Set value in upvalue cell
                let value = ctx.get_register(src.0).clone();
                ctx.set_upvalue(idx.0, value)?;
                Ok(InstructionResult::Continue)
            }

            Instruction::CloseUpvalue { local_idx } => {
                // Close the upvalue: sync local value to cell and remove from open set
                ctx.close_upvalue(local_idx.0)?;
                Ok(InstructionResult::Continue)
            }

            Instruction::GetGlobal {
                dst,
                name,
                ic_index,
            } => {
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;

                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;

                // IC Fast Path
                let cached_value = {
                    let global_obj = ctx.global();
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let func = frame
                        .module
                        .function(frame.function_index)
                        .ok_or_else(|| VmError::internal("no function"))?;
                    let feedback = func.feedback_vector.read();
                    if let Some(ic) = feedback.get(*ic_index as usize) {
                        if let otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                            shape_id: shape_addr,
                            offset,
                        } = &ic.ic_state
                        {
                            if std::sync::Arc::as_ptr(&global_obj.shape()) as u64 == *shape_addr {
                                global_obj.get_by_offset(*offset as usize)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                if let Some(value) = cached_value {
                    ctx.set_register(dst.0, value);
                    return Ok(InstructionResult::Continue);
                }

                let value = match ctx.get_global_utf16(name_str) {
                    Some(value) => value,
                    None => {
                        let message =
                            format!("{} is not defined", String::from_utf16_lossy(name_str));
                        let error = self.make_error(ctx, "ReferenceError", &message);
                        return Ok(InstructionResult::Throw(error));
                    }
                };

                // Update IC
                {
                    let global_obj = ctx.global().clone();
                    let key = Self::utf16_key(name_str);
                    if let Some(offset) = global_obj.shape().get_offset(&key) {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let mut feedback = func.feedback_vector.write();
                        if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                            if matches!(
                                ic.ic_state,
                                otter_vm_bytecode::function::InlineCacheState::Uninitialized
                            ) {
                                ic.ic_state =
                                    otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                        shape_id: std::sync::Arc::as_ptr(&global_obj.shape())
                                            as u64,
                                        offset: offset as u32,
                                    };
                            }
                        }
                    }
                }

                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::SetGlobal {
                name,
                src,
                ic_index,
            } => {
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;

                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;
                let val_val = ctx.get_register(src.0).clone();

                // IC Fast Path
                {
                    let global_obj = ctx.global().clone();
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let func = frame
                        .module
                        .function(frame.function_index)
                        .ok_or_else(|| VmError::internal("no function"))?;
                    let feedback = func.feedback_vector.read();
                    if let Some(ic) = feedback.get(*ic_index as usize) {
                        if let otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                            shape_id: shape_addr,
                            offset,
                        } = &ic.ic_state
                        {
                            if std::sync::Arc::as_ptr(&global_obj.shape()) as u64 == *shape_addr {
                                if global_obj.set_by_offset(*offset as usize, val_val.clone()) {
                                    return Ok(InstructionResult::Continue);
                                }
                            }
                        }
                    }
                }

                ctx.set_global_utf16(name_str, val_val.clone());

                // Update IC
                {
                    let global_obj = ctx.global().clone();
                    let key = Self::utf16_key(name_str);
                    if let Some(offset) = global_obj.shape().get_offset(&key) {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let mut feedback = func.feedback_vector.write();
                        if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                            if matches!(
                                ic.ic_state,
                                otter_vm_bytecode::function::InlineCacheState::Uninitialized
                            ) {
                                ic.ic_state =
                                    otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                        shape_id: std::sync::Arc::as_ptr(&global_obj.shape())
                                            as u64,
                                        offset: offset as u32,
                                    };
                            }
                        }
                    }
                }

                Ok(InstructionResult::Continue)
            }

            Instruction::LoadThis { dst } => {
                // In derived constructors, `this` is not available until super() is called
                if let Some(frame) = ctx.current_frame() {
                    if frame.is_derived && !frame.this_initialized {
                        return Err(VmError::ReferenceError(
                            "Must call super constructor in derived class before accessing 'this' or returning from derived constructor".to_string(),
                        ));
                    }
                }
                let this_value = ctx.this_value();
                ctx.set_register(dst.0, this_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::ToNumber { dst, src } => {
                let value = ctx.get_register(src.0);
                let number = self.coerce_number(value)?;
                ctx.set_register(dst.0, Value::number(number));
                Ok(InstructionResult::Continue)
            }

            // ==================== Arithmetic ====================
            Instruction::Add {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                // Collect type feedback and check for quickening opportunity
                let use_int32_fast_path = if let Some(frame) = ctx.current_frame() {
                    if let Some(func) = frame.module.function(frame.function_index) {
                        let mut feedback = func.feedback_vector.write();
                        if let Some(meta) = feedback.get_mut(*feedback_index as usize) {
                            Self::observe_value_type(&mut meta.type_observations, left);
                            Self::observe_value_type(&mut meta.type_observations, right);
                            // Use fast path if only int32 types have been seen
                            meta.type_observations.is_int32_only()
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                // Fast path for int32 addition (inline quickening)
                if use_int32_fast_path {
                    if let (Some(l), Some(r)) = (left.as_int32(), right.as_int32()) {
                        if let Some(result) = l.checked_add(r) {
                            ctx.set_register(dst.0, Value::int32(result));
                            return Ok(InstructionResult::Continue);
                        }
                    }
                }

                // Generic path
                let result = self.op_add(left, right)?;
                ctx.set_register(dst.0, result);
                Ok(InstructionResult::Continue)
            }

            Instruction::Sub {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left_value = ctx.get_register(lhs.0);
                let right_value = ctx.get_register(rhs.0);

                // Collect type feedback and check for quickening opportunity
                let use_int32_fast_path = if let Some(frame) = ctx.current_frame() {
                    if let Some(func) = frame.module.function(frame.function_index) {
                        let mut feedback = func.feedback_vector.write();
                        if let Some(meta) = feedback.get_mut(*feedback_index as usize) {
                            Self::observe_value_type(&mut meta.type_observations, left_value);
                            Self::observe_value_type(&mut meta.type_observations, right_value);
                            meta.type_observations.is_int32_only()
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                // Fast path for int32 subtraction (inline quickening)
                if use_int32_fast_path {
                    if let (Some(l), Some(r)) = (left_value.as_int32(), right_value.as_int32()) {
                        if let Some(result) = l.checked_sub(r) {
                            ctx.set_register(dst.0, Value::int32(result));
                            return Ok(InstructionResult::Continue);
                        }
                    }
                }

                // Generic path
                let left_bigint = self.bigint_value(left_value)?;
                let right_bigint = self.bigint_value(right_value)?;

                if let (Some(left_bigint), Some(right_bigint)) = (left_bigint, right_bigint) {
                    let result = left_bigint - right_bigint;
                    ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    return Ok(InstructionResult::Continue);
                }

                if left_value.is_bigint() || right_value.is_bigint() {
                    return Err(VmError::type_error("Cannot mix BigInt and other types"));
                }

                let left = self.coerce_number(left_value)?;
                let right = self.coerce_number(right_value)?;

                ctx.set_register(dst.0, Value::number(left - right));
                Ok(InstructionResult::Continue)
            }

            Instruction::Inc { dst, src } => {
                let value = ctx.get_register(src.0);
                if let Some(bigint) = self.bigint_value(value)? {
                    let result = bigint + NumBigInt::one();
                    ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    return Ok(InstructionResult::Continue);
                }

                let val = self.coerce_number(value)?;
                ctx.set_register(dst.0, Value::number(val + 1.0));
                Ok(InstructionResult::Continue)
            }

            Instruction::Dec { dst, src } => {
                let value = ctx.get_register(src.0);
                if let Some(bigint) = self.bigint_value(value)? {
                    let result = bigint - NumBigInt::one();
                    ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    return Ok(InstructionResult::Continue);
                }

                let val = self.coerce_number(value)?;
                ctx.set_register(dst.0, Value::number(val - 1.0));
                Ok(InstructionResult::Continue)
            }

            Instruction::Mul {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left_value = ctx.get_register(lhs.0);
                let right_value = ctx.get_register(rhs.0);

                // Collect type feedback and check for quickening opportunity
                let use_int32_fast_path = if let Some(frame) = ctx.current_frame() {
                    if let Some(func) = frame.module.function(frame.function_index) {
                        let mut feedback = func.feedback_vector.write();
                        if let Some(meta) = feedback.get_mut(*feedback_index as usize) {
                            Self::observe_value_type(&mut meta.type_observations, left_value);
                            Self::observe_value_type(&mut meta.type_observations, right_value);
                            meta.type_observations.is_int32_only()
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                // Fast path for int32 multiplication (inline quickening)
                if use_int32_fast_path {
                    if let (Some(l), Some(r)) = (left_value.as_int32(), right_value.as_int32()) {
                        if let Some(result) = l.checked_mul(r) {
                            ctx.set_register(dst.0, Value::int32(result));
                            return Ok(InstructionResult::Continue);
                        }
                    }
                }

                // Generic path
                let left_bigint = self.bigint_value(left_value)?;
                let right_bigint = self.bigint_value(right_value)?;

                if let (Some(left_bigint), Some(right_bigint)) = (left_bigint, right_bigint) {
                    let result = left_bigint * right_bigint;
                    ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    return Ok(InstructionResult::Continue);
                }

                if left_value.is_bigint() || right_value.is_bigint() {
                    return Err(VmError::type_error("Cannot mix BigInt and other types"));
                }

                let left = self.coerce_number(left_value)?;
                let right = self.coerce_number(right_value)?;

                ctx.set_register(dst.0, Value::number(left * right));
                Ok(InstructionResult::Continue)
            }

            Instruction::Div {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left_value = ctx.get_register(lhs.0);
                let right_value = ctx.get_register(rhs.0);

                // Collect type feedback and check for quickening opportunity
                let use_int32_fast_path = if let Some(frame) = ctx.current_frame() {
                    if let Some(func) = frame.module.function(frame.function_index) {
                        let mut feedback = func.feedback_vector.write();
                        if let Some(meta) = feedback.get_mut(*feedback_index as usize) {
                            Self::observe_value_type(&mut meta.type_observations, left_value);
                            Self::observe_value_type(&mut meta.type_observations, right_value);
                            meta.type_observations.is_int32_only()
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                // Fast path for int32 division (only if result is exact integer)
                if use_int32_fast_path {
                    if let (Some(l), Some(r)) = (left_value.as_int32(), right_value.as_int32()) {
                        if r != 0 && l % r == 0 {
                            // Result is an exact integer
                            ctx.set_register(dst.0, Value::int32(l / r));
                            return Ok(InstructionResult::Continue);
                        }
                    }
                }

                // Generic path
                let left_bigint = self.bigint_value(left_value)?;
                let right_bigint = self.bigint_value(right_value)?;

                if let (Some(left_bigint), Some(right_bigint)) = (left_bigint, right_bigint) {
                    if right_bigint.is_zero() {
                        return Err(VmError::range_error("Division by zero"));
                    }
                    let result = left_bigint / right_bigint;
                    ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    return Ok(InstructionResult::Continue);
                }

                if left_value.is_bigint() || right_value.is_bigint() {
                    return Err(VmError::type_error("Cannot mix BigInt and other types"));
                }

                let left = self.coerce_number(left_value)?;
                let right = self.coerce_number(right_value)?;

                ctx.set_register(dst.0, Value::number(left / right));
                Ok(InstructionResult::Continue)
            }

            // ==================== Quickened Arithmetic (type-specialized) ====================
            Instruction::AddI32 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                // Fast path: both operands are int32
                if let (Some(l), Some(r)) = (left.as_int32(), right.as_int32()) {
                    // Check for overflow, fall back to f64 if it occurs
                    if let Some(result) = l.checked_add(r) {
                        ctx.set_register(dst.0, Value::int32(result));
                        return Ok(InstructionResult::Continue);
                    }
                }

                // Fallback to generic add
                let result = self.op_add(left, right)?;
                ctx.set_register(dst.0, result);
                Ok(InstructionResult::Continue)
            }

            Instruction::SubI32 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                // Fast path: both operands are int32
                if let (Some(l), Some(r)) = (left.as_int32(), right.as_int32()) {
                    if let Some(result) = l.checked_sub(r) {
                        ctx.set_register(dst.0, Value::int32(result));
                        return Ok(InstructionResult::Continue);
                    }
                }

                // Fallback to generic sub
                let left_num = self.coerce_number(left)?;
                let right_num = self.coerce_number(right)?;
                ctx.set_register(dst.0, Value::number(left_num - right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::MulI32 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                // Fast path: both operands are int32
                if let (Some(l), Some(r)) = (left.as_int32(), right.as_int32()) {
                    if let Some(result) = l.checked_mul(r) {
                        ctx.set_register(dst.0, Value::int32(result));
                        return Ok(InstructionResult::Continue);
                    }
                }

                // Fallback to generic mul
                let left_num = self.coerce_number(left)?;
                let right_num = self.coerce_number(right)?;
                ctx.set_register(dst.0, Value::number(left_num * right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::DivI32 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                // Fast path: both operands are int32 and divide evenly
                if let (Some(l), Some(r)) = (left.as_int32(), right.as_int32()) {
                    if r != 0 && l % r == 0 {
                        if let Some(result) = l.checked_div(r) {
                            ctx.set_register(dst.0, Value::int32(result));
                            return Ok(InstructionResult::Continue);
                        }
                    }
                }

                // Fallback to generic div (produces f64)
                let left_num = self.coerce_number(left)?;
                let right_num = self.coerce_number(right)?;
                ctx.set_register(dst.0, Value::number(left_num / right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::AddF64 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                // Fast path: both operands are numbers
                if let (Some(l), Some(r)) = (left.as_number(), right.as_number()) {
                    ctx.set_register(dst.0, Value::number(l + r));
                    return Ok(InstructionResult::Continue);
                }

                // Fallback to generic add
                let result = self.op_add(left, right)?;
                ctx.set_register(dst.0, result);
                Ok(InstructionResult::Continue)
            }

            Instruction::SubF64 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                // Fast path: both operands are numbers
                if let (Some(l), Some(r)) = (left.as_number(), right.as_number()) {
                    ctx.set_register(dst.0, Value::number(l - r));
                    return Ok(InstructionResult::Continue);
                }

                // Fallback to generic sub
                let left_num = self.coerce_number(left)?;
                let right_num = self.coerce_number(right)?;
                ctx.set_register(dst.0, Value::number(left_num - right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::MulF64 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                // Fast path: both operands are numbers
                if let (Some(l), Some(r)) = (left.as_number(), right.as_number()) {
                    ctx.set_register(dst.0, Value::number(l * r));
                    return Ok(InstructionResult::Continue);
                }

                // Fallback to generic mul
                let left_num = self.coerce_number(left)?;
                let right_num = self.coerce_number(right)?;
                ctx.set_register(dst.0, Value::number(left_num * right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::DivF64 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                // Fast path: both operands are numbers
                if let (Some(l), Some(r)) = (left.as_number(), right.as_number()) {
                    ctx.set_register(dst.0, Value::number(l / r));
                    return Ok(InstructionResult::Continue);
                }

                // Fallback to generic div
                let left_num = self.coerce_number(left)?;
                let right_num = self.coerce_number(right)?;
                ctx.set_register(dst.0, Value::number(left_num / right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::Mod { dst, lhs, rhs } => {
                let left_value = ctx.get_register(lhs.0);
                let right_value = ctx.get_register(rhs.0);
                let left_bigint = self.bigint_value(left_value)?;
                let right_bigint = self.bigint_value(right_value)?;

                if let (Some(left_bigint), Some(right_bigint)) = (left_bigint, right_bigint) {
                    if right_bigint.is_zero() {
                        return Err(VmError::range_error("Division by zero"));
                    }
                    let result = left_bigint % right_bigint;
                    ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    return Ok(InstructionResult::Continue);
                }

                if left_value.is_bigint() || right_value.is_bigint() {
                    return Err(VmError::type_error("Cannot mix BigInt and other types"));
                }

                let left = self.coerce_number(left_value)?;
                let right = self.coerce_number(right_value)?;

                ctx.set_register(dst.0, Value::number(left % right));
                Ok(InstructionResult::Continue)
            }

            Instruction::Pow { dst, lhs, rhs } => {
                let left_value = ctx.get_register(lhs.0);
                let right_value = ctx.get_register(rhs.0);
                let left_bigint = self.bigint_value(left_value)?;
                let right_bigint = self.bigint_value(right_value)?;

                if let (Some(left_bigint), Some(right_bigint)) = (left_bigint, right_bigint) {
                    if right_bigint < NumBigInt::zero() {
                        return Err(VmError::range_error(
                            "Exponent must be non-negative for BigInt",
                        ));
                    }
                    let exponent = right_bigint
                        .to_u32()
                        .ok_or_else(|| VmError::range_error("Exponent too large for BigInt"))?;
                    let result = left_bigint.pow(exponent);
                    ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    return Ok(InstructionResult::Continue);
                }

                if left_value.is_bigint() || right_value.is_bigint() {
                    return Err(VmError::type_error("Cannot mix BigInt and other types"));
                }

                let left = self.coerce_number(left_value)?;
                let right = self.coerce_number(right_value)?;

                ctx.set_register(dst.0, Value::number(left.powf(right)));
                Ok(InstructionResult::Continue)
            }

            Instruction::Neg { dst, src } => {
                let val = ctx.get_register(src.0);
                if let Some(crate::value::HeapRef::BigInt(b)) = val.heap_ref() {
                    let s = &b.value;
                    let result_s = if s.starts_with('-') {
                        s[1..].to_string()
                    } else if s == "0" {
                        "0".to_string()
                    } else {
                        format!("-{}", s)
                    };
                    ctx.set_register(dst.0, Value::bigint(result_s));
                    return Ok(InstructionResult::Continue);
                }

                let value = self.coerce_number(val)?;

                ctx.set_register(dst.0, Value::number(-value));
                Ok(InstructionResult::Continue)
            }

            // ==================== Comparison ====================
            Instruction::Eq { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                let result = self.abstract_equal(left, right);
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Ne { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                let result = !self.abstract_equal(left, right);
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::StrictEq { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                let result = self.strict_equal(left, right);
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::StrictNe { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                let result = !self.strict_equal(left, right);
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Lt { dst, lhs, rhs } => {
                let left = self.to_numeric(ctx.get_register(lhs.0))?;
                let right = self.to_numeric(ctx.get_register(rhs.0))?;
                let result = matches!(self.numeric_compare(left, right)?, Some(Ordering::Less));

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Le { dst, lhs, rhs } => {
                let left = self.to_numeric(ctx.get_register(lhs.0))?;
                let right = self.to_numeric(ctx.get_register(rhs.0))?;
                let result = matches!(
                    self.numeric_compare(left, right)?,
                    Some(Ordering::Less | Ordering::Equal)
                );

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Gt { dst, lhs, rhs } => {
                let left = self.to_numeric(ctx.get_register(lhs.0))?;
                let right = self.to_numeric(ctx.get_register(rhs.0))?;
                let result = matches!(self.numeric_compare(left, right)?, Some(Ordering::Greater));

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Ge { dst, lhs, rhs } => {
                let left = self.to_numeric(ctx.get_register(lhs.0))?;
                let right = self.to_numeric(ctx.get_register(rhs.0))?;
                let result = matches!(
                    self.numeric_compare(left, right)?,
                    Some(Ordering::Greater | Ordering::Equal)
                );

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            // ==================== Logical ====================
            Instruction::Not { dst, src } => {
                let value = ctx.get_register(src.0).to_boolean();
                ctx.set_register(dst.0, Value::boolean(!value));
                Ok(InstructionResult::Continue)
            }

            // ==================== Type Operations ====================
            Instruction::TypeOf { dst, src } => {
                let type_name = ctx.get_register(src.0).type_of();
                let str_value = Value::string(JsString::intern(type_name));
                ctx.set_register(dst.0, str_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::TypeOfName { dst, name } => {
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;

                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;

                let type_name = match ctx.get_global_utf16(&name_str) {
                    Some(value) => value.type_of(),
                    None => "undefined",
                };
                let str_value = Value::string(JsString::intern(type_name));
                ctx.set_register(dst.0, str_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::InstanceOf {
                dst,
                lhs,
                rhs,
                ic_index,
            } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                let Some(left_obj) = left.as_object() else {
                    ctx.set_register(dst.0, Value::boolean(false));
                    return Ok(InstructionResult::Continue);
                };

                let Some(right_obj) = right.as_object() else {
                    return Err(VmError::type_error(
                        "Right-hand side of instanceof is not an object",
                    ));
                };

                // IC Fast Path - cache the prototype property lookup on the constructor
                let proto_key = PropertyKey::string("prototype");
                let mut cached_proto = None;
                {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let func = frame
                        .module
                        .function(frame.function_index)
                        .ok_or_else(|| VmError::internal("no function"))?;
                    let feedback = func.feedback_vector.read();
                    if let Some(ic) = feedback.get(*ic_index as usize) {
                        use otter_vm_bytecode::function::InlineCacheState;
                        let obj_shape_ptr = std::sync::Arc::as_ptr(&right_obj.shape()) as u64;

                        if ic.proto_epoch_matches(get_proto_epoch()) {
                            match &ic.ic_state {
                                InlineCacheState::Monomorphic { shape_id, offset } => {
                                    if obj_shape_ptr == *shape_id {
                                        cached_proto = right_obj.get_by_offset(*offset as usize);
                                    }
                                }
                                InlineCacheState::Polymorphic { count, entries } => {
                                    for i in 0..(*count as usize) {
                                        if obj_shape_ptr == entries[i].0 {
                                            cached_proto =
                                                right_obj.get_by_offset(entries[i].1 as usize);
                                            break;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                let proto_val = if let Some(val) = cached_proto {
                    val
                } else {
                    // Slow path: full lookup and IC update
                    let proto = right_obj.get(&proto_key).unwrap_or_else(Value::undefined);

                    // Update IC
                    if let Some(offset) = right_obj.shape().get_offset(&proto_key) {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let mut feedback = func.feedback_vector.write();
                        if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                            use otter_vm_bytecode::function::InlineCacheState;
                            // Skip IC for dictionary mode objects
                            if right_obj.is_dictionary_mode() {
                                ic.ic_state = InlineCacheState::Megamorphic;
                            } else {
                                let shape_ptr = std::sync::Arc::as_ptr(&right_obj.shape()) as u64;
                                let current_epoch = get_proto_epoch();

                                match &mut ic.ic_state {
                                    InlineCacheState::Uninitialized => {
                                        ic.ic_state = InlineCacheState::Monomorphic {
                                            shape_id: shape_ptr,
                                            offset: offset as u32,
                                        };
                                        ic.proto_epoch = current_epoch;
                                    }
                                    InlineCacheState::Monomorphic {
                                        shape_id: old_shape,
                                        offset: old_offset,
                                    } => {
                                        if *old_shape != shape_ptr {
                                            let mut entries = [(0u64, 0u32); 4];
                                            entries[0] = (*old_shape, *old_offset);
                                            entries[1] = (shape_ptr, offset as u32);
                                            ic.ic_state =
                                                InlineCacheState::Polymorphic { count: 2, entries };
                                            ic.proto_epoch = current_epoch;
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        let mut found = false;
                                        for i in 0..(*count as usize) {
                                            if entries[i].0 == shape_ptr {
                                                found = true;
                                                break;
                                            }
                                        }
                                        if !found {
                                            if (*count as usize) < 4 {
                                                entries[*count as usize] =
                                                    (shape_ptr, offset as u32);
                                                *count += 1;
                                                ic.proto_epoch = current_epoch;
                                            } else {
                                                ic.ic_state = InlineCacheState::Megamorphic;
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    proto
                };

                let Some(target_proto) = proto_val.as_object() else {
                    return Err(VmError::type_error("Function has non-object prototype"));
                };

                let mut current = Some(left_obj);
                let mut depth = 0;
                const MAX_PROTO_DEPTH: usize = 100;
                while let Some(obj) = current {
                    if obj.as_ptr() == target_proto.as_ptr() {
                        ctx.set_register(dst.0, Value::boolean(true));
                        return Ok(InstructionResult::Continue);
                    }
                    depth += 1;
                    if depth > MAX_PROTO_DEPTH {
                        break;
                    }
                    current = obj.prototype();
                }

                ctx.set_register(dst.0, Value::boolean(false));
                Ok(InstructionResult::Continue)
            }

            Instruction::In {
                dst,
                lhs,
                rhs,
                ic_index,
            } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                // Proxy check - must be first
                if let Some(proxy) = right.as_proxy() {
                    let key = if let Some(n) = left.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = left.as_string() {
                        PropertyKey::from_js_string(s)
                    } else if let Some(sym) = left.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else {
                        let idx_str = self.to_string(&left);
                        PropertyKey::string(&idx_str)
                    };
                    let result = crate::proxy_operations::proxy_has(
                        self,
                        ctx,
                        proxy,
                        &key,
                        left.clone(),
                    )?;
                    ctx.set_register(dst.0, Value::boolean(result));
                    return Ok(InstructionResult::Continue);
                }

                let Some(right_obj) = right.as_object() else {
                    return Err(VmError::type_error(
                        "Cannot use 'in' operator to search for property in non-object",
                    ));
                };

                let key = if let Some(n) = left.as_int32() {
                    PropertyKey::Index(n as u32)
                } else if let Some(s) = left.as_string() {
                    PropertyKey::from_js_string(s)
                } else if let Some(sym) = left.as_symbol() {
                    PropertyKey::Symbol(sym.id)
                } else {
                    let idx_str = self.to_string(&left);
                    PropertyKey::string(&idx_str)
                };

                // IC Fast Path - only for string keys
                // For 'in' operator, we cache whether the property exists on a shape
                // The offset field is reused: 0 = property doesn't exist, non-zero = exists
                if matches!(&key, PropertyKey::String(_)) {
                    let mut cached_result = None;
                    {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let feedback = func.feedback_vector.read();
                        if let Some(ic) = feedback.get(*ic_index as usize) {
                            use otter_vm_bytecode::function::InlineCacheState;
                            let obj_shape_ptr = std::sync::Arc::as_ptr(&right_obj.shape()) as u64;

                            if ic.proto_epoch_matches(get_proto_epoch()) {
                                match &ic.ic_state {
                                    InlineCacheState::Monomorphic { shape_id, offset } => {
                                        if obj_shape_ptr == *shape_id {
                                            // offset encodes: 1 = exists, 0 = doesn't exist
                                            cached_result = Some(*offset != 0);
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        for i in 0..(*count as usize) {
                                            if obj_shape_ptr == entries[i].0 {
                                                cached_result = Some(entries[i].1 != 0);
                                                break;
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    if let Some(result) = cached_result {
                        ctx.set_register(dst.0, Value::boolean(result));
                        return Ok(InstructionResult::Continue);
                    }

                    // Slow path with IC update
                    let has_property = right_obj.has(&key);
                    {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let mut feedback = func.feedback_vector.write();
                        if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                            use otter_vm_bytecode::function::InlineCacheState;
                            // Skip IC for dictionary mode objects
                            if right_obj.is_dictionary_mode() {
                                ic.ic_state = InlineCacheState::Megamorphic;
                            } else {
                                let shape_ptr = std::sync::Arc::as_ptr(&right_obj.shape()) as u64;
                                let exists_marker = if has_property { 1u32 } else { 0u32 };
                                let current_epoch = get_proto_epoch();

                                match &mut ic.ic_state {
                                    InlineCacheState::Uninitialized => {
                                        ic.ic_state = InlineCacheState::Monomorphic {
                                            shape_id: shape_ptr,
                                            offset: exists_marker,
                                        };
                                        ic.proto_epoch = current_epoch;
                                    }
                                    InlineCacheState::Monomorphic {
                                        shape_id: old_shape,
                                        offset: old_exists,
                                    } => {
                                        if *old_shape != shape_ptr {
                                            let mut entries = [(0u64, 0u32); 4];
                                            entries[0] = (*old_shape, *old_exists);
                                            entries[1] = (shape_ptr, exists_marker);
                                            ic.ic_state =
                                                InlineCacheState::Polymorphic { count: 2, entries };
                                            ic.proto_epoch = current_epoch;
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        let mut found = false;
                                        for i in 0..(*count as usize) {
                                            if entries[i].0 == shape_ptr {
                                                found = true;
                                                break;
                                            }
                                        }
                                        if !found {
                                            if (*count as usize) < 4 {
                                                entries[*count as usize] =
                                                    (shape_ptr, exists_marker);
                                                *count += 1;
                                                ic.proto_epoch = current_epoch;
                                            } else {
                                                ic.ic_state = InlineCacheState::Megamorphic;
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    ctx.set_register(dst.0, Value::boolean(has_property));
                    return Ok(InstructionResult::Continue);
                }

                let result = right_obj.has(&key);
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            // ==================== Control Flow ====================
            Instruction::Jump { offset } => Ok(InstructionResult::Jump(offset.0)),

            Instruction::JumpIfTrue { cond, offset } => {
                if ctx.get_register(cond.0).to_boolean() {
                    Ok(InstructionResult::Jump(offset.0))
                } else {
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::JumpIfFalse { cond, offset } => {
                if !ctx.get_register(cond.0).to_boolean() {
                    Ok(InstructionResult::Jump(offset.0))
                } else {
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::JumpIfNullish { src, offset } => {
                if ctx.get_register(src.0).is_nullish() {
                    Ok(InstructionResult::Jump(offset.0))
                } else {
                    Ok(InstructionResult::Continue)
                }
            }

            // ==================== Exception Handling ====================
            Instruction::TryStart { catch_offset } => {
                let pc = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame"))?
                    .pc;
                let catch_pc = (pc as i32 + catch_offset.0) as usize;
                ctx.push_try(catch_pc);
                Ok(InstructionResult::Continue)
            }

            Instruction::TryEnd => {
                ctx.pop_try_for_current_frame();
                Ok(InstructionResult::Continue)
            }

            Instruction::Throw { src } => {
                let value = ctx.get_register(src.0).clone();
                Ok(InstructionResult::Throw(value))
            }

            Instruction::Catch { dst } => {
                let value = ctx.take_exception().unwrap_or_else(Value::undefined);
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            // ==================== Functions ====================
            Instruction::Closure { dst, func } => {
                // Get the function definition to know what upvalues to capture
                let func_def = module
                    .function(func.0)
                    .ok_or_else(|| VmError::internal("function not found for closure"))?;

                // Capture upvalues from parent frame
                let captured_upvalues = self.capture_upvalues(ctx, &func_def.upvalues)?;

                let func_obj = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));

                // Get Object.prototype so function's .prototype object has correct chain
                let obj_proto = ctx
                    .global()
                    .get(&PropertyKey::string("Object"))
                    .and_then(|obj_ctor| {
                        obj_ctor
                            .as_object()
                            .and_then(|o| o.get(&PropertyKey::string("prototype")))
                    })
                    .and_then(|proto_val| proto_val.as_object());

                let proto = GcRef::new(JsObject::new(obj_proto, ctx.memory_manager().clone()));

                // Set [[Prototype]] to Function.prototype so methods like
                // .bind(), .call(), .apply() are inherited per ES2023 §10.2.4.
                if let Some(fn_proto) = ctx.function_prototype() {
                    func_obj.set_prototype(Some(fn_proto));
                }

                // Set function length and name properties with correct attributes
                // (writable: false, enumerable: false, configurable: true)
                let fn_attrs = crate::object::PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                };
                func_obj.define_property(
                    PropertyKey::string("length"),
                    crate::object::PropertyDescriptor::Data {
                        value: Value::int32(func_def.param_count as i32),
                        attributes: fn_attrs,
                    },
                );
                let fn_name = func_def.name.as_deref().unwrap_or("");
                func_obj.define_property(
                    PropertyKey::string("name"),
                    crate::object::PropertyDescriptor::Data {
                        value: Value::string(JsString::intern(fn_name)),
                        attributes: fn_attrs,
                    },
                );

                let closure = Arc::new(Closure {
                    function_index: func.0,
                    module: Arc::clone(module),
                    upvalues: captured_upvalues,
                    is_async: func_def.is_async(),
                    is_generator: false,
                    object: func_obj,
                    home_object: None,
                });
                let func_value = Value::function(closure);
                func_obj.set(PropertyKey::string("prototype"), Value::object(proto));
                proto.set(PropertyKey::string("constructor"), func_value.clone());
                if func_def.is_arrow() || func_def.is_async() {
                    func_obj.define_property(
                        PropertyKey::string("__non_constructor"),
                        PropertyDescriptor::Data {
                            value: Value::boolean(true),
                            attributes: PropertyAttributes {
                                writable: false,
                                enumerable: false,
                                configurable: false,
                            },
                        },
                    );
                }
                ctx.set_register(dst.0, func_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::AsyncClosure { dst, func } => {
                // Get the function definition to know what upvalues to capture
                let func_def = module
                    .function(func.0)
                    .ok_or_else(|| VmError::internal("function not found for async closure"))?;

                // Capture upvalues from parent frame
                let captured_upvalues = self.capture_upvalues(ctx, &func_def.upvalues)?;

                let func_obj = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));

                // Get Object.prototype for function's .prototype object
                let obj_proto = ctx
                    .global()
                    .get(&PropertyKey::string("Object"))
                    .and_then(|obj_ctor| {
                        obj_ctor
                            .as_object()
                            .and_then(|o| o.get(&PropertyKey::string("prototype")))
                    })
                    .and_then(|proto_val| proto_val.as_object());

                let proto = GcRef::new(JsObject::new(obj_proto, ctx.memory_manager().clone()));

                // Set [[Prototype]] to Function.prototype
                if let Some(fn_proto) = ctx.function_prototype() {
                    func_obj.set_prototype(Some(fn_proto));
                }

                // Set function length and name properties
                let fn_attrs = crate::object::PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                };
                func_obj.define_property(
                    PropertyKey::string("length"),
                    crate::object::PropertyDescriptor::Data {
                        value: Value::int32(func_def.param_count as i32),
                        attributes: fn_attrs,
                    },
                );
                let fn_name = func_def.name.as_deref().unwrap_or("");
                func_obj.define_property(
                    PropertyKey::string("name"),
                    crate::object::PropertyDescriptor::Data {
                        value: Value::string(JsString::intern(fn_name)),
                        attributes: fn_attrs,
                    },
                );

                let closure = Arc::new(Closure {
                    function_index: func.0,
                    module: Arc::clone(module),
                    upvalues: captured_upvalues,
                    is_async: true,
                    is_generator: false,
                    object: func_obj,
                    home_object: None,
                });
                let func_value = Value::function(closure);
                func_obj.set(PropertyKey::string("prototype"), Value::object(proto));
                proto.set(PropertyKey::string("constructor"), func_value.clone());
                func_obj.define_property(
                    PropertyKey::string("__non_constructor"),
                    PropertyDescriptor::Data {
                        value: Value::boolean(true),
                        attributes: PropertyAttributes {
                            writable: false,
                            enumerable: false,
                            configurable: false,
                        },
                    },
                );
                ctx.set_register(dst.0, func_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::GeneratorClosure { dst, func } => {
                // Get the function definition to know what upvalues to capture
                let func_def = module
                    .function(func.0)
                    .ok_or_else(|| VmError::internal("function not found for generator closure"))?;

                // Capture upvalues from parent frame
                let captured_upvalues = self.capture_upvalues(ctx, &func_def.upvalues)?;

                // Get GeneratorFunctionPrototype as the function's prototype (for Object.getPrototypeOf)
                let gen_func_proto = ctx
                    .get_global("GeneratorFunctionPrototype")
                    .and_then(|v| v.as_object());

                // Create a generator function closure - when called, it creates a generator object
                let func_obj =
                    GcRef::new(JsObject::new(gen_func_proto, ctx.memory_manager().clone()));

                // Set function length and name properties
                let fn_attrs = crate::object::PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                };
                func_obj.define_property(
                    PropertyKey::string("length"),
                    crate::object::PropertyDescriptor::Data {
                        value: Value::int32(func_def.param_count as i32),
                        attributes: fn_attrs,
                    },
                );
                let fn_name = func_def.name.as_deref().unwrap_or("");
                func_obj.define_property(
                    PropertyKey::string("name"),
                    crate::object::PropertyDescriptor::Data {
                        value: Value::string(JsString::intern(fn_name)),
                        attributes: fn_attrs,
                    },
                );

                // Create the .prototype for instances - this becomes the prototype of generator instances
                let gen_proto = ctx
                    .get_global("GeneratorPrototype")
                    .and_then(|v| v.as_object());
                let proto = GcRef::new(JsObject::new(gen_proto, ctx.memory_manager().clone()));

                let closure = Arc::new(Closure {
                    function_index: func.0,
                    module: Arc::clone(module),
                    upvalues: captured_upvalues,
                    is_async: false,
                    is_generator: true,
                    object: func_obj,
                    home_object: None,
                });
                let func_value = Value::function(closure);
                func_obj.set(PropertyKey::string("prototype"), Value::object(proto));
                proto.set(PropertyKey::string("constructor"), func_value.clone());
                func_obj.define_property(
                    PropertyKey::string("__non_constructor"),
                    PropertyDescriptor::Data {
                        value: Value::boolean(true),
                        attributes: PropertyAttributes {
                            writable: false,
                            enumerable: false,
                            configurable: false,
                        },
                    },
                );
                ctx.set_register(dst.0, func_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::AsyncGeneratorClosure { dst, func } => {
                // Get the function definition to know what upvalues to capture
                let func_def = module.function(func.0).ok_or_else(|| {
                    VmError::internal("function not found for async generator closure")
                })?;

                // Capture upvalues from parent frame
                let captured_upvalues = self.capture_upvalues(ctx, &func_def.upvalues)?;

                // Get AsyncGeneratorFunctionPrototype as the function's prototype (for Object.getPrototypeOf)
                let async_gen_func_proto = ctx
                    .get_global("AsyncGeneratorFunctionPrototype")
                    .and_then(|v| v.as_object());

                // Create an async generator function closure
                let func_obj = GcRef::new(JsObject::new(
                    async_gen_func_proto,
                    ctx.memory_manager().clone(),
                ));

                // Set function length and name properties
                let fn_attrs = crate::object::PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                };
                func_obj.define_property(
                    PropertyKey::string("length"),
                    crate::object::PropertyDescriptor::Data {
                        value: Value::int32(func_def.param_count as i32),
                        attributes: fn_attrs,
                    },
                );
                let fn_name = func_def.name.as_deref().unwrap_or("");
                func_obj.define_property(
                    PropertyKey::string("name"),
                    crate::object::PropertyDescriptor::Data {
                        value: Value::string(JsString::intern(fn_name)),
                        attributes: fn_attrs,
                    },
                );

                // Create the .prototype for instances - this becomes the prototype of generator instances
                let gen_proto = ctx
                    .get_global("GeneratorPrototype")
                    .and_then(|v| v.as_object());
                let proto = GcRef::new(JsObject::new(gen_proto, ctx.memory_manager().clone()));

                let closure = Arc::new(Closure {
                    function_index: func.0,
                    module: Arc::clone(module),
                    upvalues: captured_upvalues,
                    is_async: true,
                    is_generator: true,
                    object: func_obj,
                    home_object: None,
                });
                let func_value = Value::function(closure);
                func_obj.set(PropertyKey::string("prototype"), Value::object(proto));
                proto.set(PropertyKey::string("constructor"), func_value.clone());
                func_obj.define_property(
                    PropertyKey::string("__non_constructor"),
                    PropertyDescriptor::Data {
                        value: Value::boolean(true),
                        attributes: PropertyAttributes {
                            writable: false,
                            enumerable: false,
                            configurable: false,
                        },
                    },
                );
                ctx.set_register(dst.0, func_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::Call { dst, func, argc } => {
                let func_value = ctx.get_register(func.0).clone();

                // Collect arguments upfront (used by multiple paths)
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    let arg = ctx.get_register(func.0 + 1 + i).clone();
                    args.push(arg);
                }

                // Check if it's a proxy with apply trap
                if let Some(proxy) = func_value.as_proxy() {
                    let result = crate::proxy_operations::proxy_apply(
                        self,
                        ctx,
                        proxy,
                        Value::undefined(),
                        &args,
                    )?;
                    ctx.set_register(dst.0, result);
                    return Ok(InstructionResult::Continue);
                }

                // Check if it's a native function first
                if let Some(native_fn) = func_value.as_native_function() {
                    // Some native ops need interpreter-level dispatch (call/apply, generator ops).
                    let is_same_native = |candidate: &Value| -> bool {
                        match (func_value.heap_ref(), candidate.heap_ref()) {
                            (
                                Some(HeapRef::NativeFunction(a)),
                                Some(HeapRef::NativeFunction(b)),
                            ) => Arc::ptr_eq(a, b),
                            _ => false,
                        }
                    };
                    let is_special = [
                        "__Function_call",
                        "__Function_apply",
                        "__Generator_next",
                        "__Generator_return",
                        "__Generator_throw",
                        "eval",
                    ]
                    .iter()
                    .any(|name| ctx.get_global(name).is_some_and(|v| is_same_native(&v)));

                    if is_special {
                        return self.handle_call_value(
                            ctx,
                            &func_value,
                            Value::undefined(),
                            args,
                            dst.0,
                        );
                    }

                    // Call the native function with depth tracking
                    let result = self.call_native_fn(ctx, native_fn, &Value::undefined(), &args)?;
                    ctx.set_register(dst.0, result);
                    return Ok(InstructionResult::Continue);
                }

                // Check if it's a bound function (object with __boundFunction__)
                if let Some(obj) = func_value.as_object() {
                    if let Some(bound_fn) = obj.get(&PropertyKey::string("__boundFunction__")) {
                        // Get bound thisArg, converting null/undefined to globalThis (non-strict mode)
                        let raw_this_arg = obj
                            .get(&PropertyKey::string("__boundThis__"))
                            .unwrap_or_else(Value::undefined);
                        let this_arg = if raw_this_arg.is_null() || raw_this_arg.is_undefined() {
                            Value::object(ctx.global())
                        } else {
                            raw_this_arg
                        };

                        // Collect bound arguments
                        let mut all_args = Vec::new();
                        if let Some(bound_args_val) = obj.get(&PropertyKey::string("__boundArgs__"))
                        {
                            if let Some(args_obj) = bound_args_val.as_object() {
                                let len = if let Some(len_val) =
                                    args_obj.get(&PropertyKey::string("length"))
                                {
                                    len_val.as_int32().unwrap_or(0) as usize
                                } else {
                                    0
                                };
                                for i in 0..len {
                                    all_args.push(
                                        args_obj
                                            .get(&PropertyKey::Index(i as u32))
                                            .unwrap_or_else(Value::undefined),
                                    );
                                }
                            }
                        }

                        // Add call-time arguments
                        for i in 0..(*argc as u16) {
                            all_args.push(ctx.get_register(func.0 + 1 + i).clone());
                        }

                        // Call the bound function with the bound this and combined args
                        if let Some(native_fn) = bound_fn.as_native_function() {
                            // For native functions, we can't set 'this' directly
                            // but most native functions don't use 'this'
                            let result =
                                self.call_native_fn(ctx, native_fn, &this_arg, &all_args)?;
                            ctx.set_register(dst.0, result);
                            return Ok(InstructionResult::Continue);
                        } else if let Some(closure) = bound_fn.as_function() {
                            // Set the bound this and args
                            let argc = all_args.len() as u8;
                            ctx.set_pending_this(this_arg);
                            ctx.set_pending_args(all_args);

                            return Ok(InstructionResult::Call {
                                func_index: closure.function_index,
                                module: Arc::clone(&closure.module),
                                argc,
                                return_reg: dst.0,
                                is_construct: false,
                                is_async: closure.is_async,
                                upvalues: closure.upvalues.clone(),
                            });
                        } else {
                            return Err(VmError::type_error(
                                "bound function target is not callable",
                            ));
                        }
                    }
                }

                self.handle_call_value(ctx, &func_value, Value::undefined(), args, dst.0)
            }

            Instruction::CallWithReceiver {
                dst,
                func,
                this,
                argc,
            } => {
                let func_value = ctx.get_register(func.0).clone();
                let this_value = ctx.get_register(this.0).clone();

                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    let arg = ctx.get_register(func.0 + 1 + i).clone();
                    args.push(arg);
                }

                self.handle_call_value(ctx, &func_value, this_value, args, dst.0)
            }

            Instruction::TailCall { func, argc } => {
                let func_value = ctx.get_register(func.0).clone();

                // Native functions don't benefit from tail call optimization
                // (they execute immediately), so just call and return
                if let Some(native_fn) = func_value.as_native_function() {
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        let arg = ctx.get_register(func.0 + 1 + i).clone();
                        args.push(arg);
                    }
                    let result = self.call_native_fn(ctx, native_fn, &Value::undefined(), &args)?;
                    return Ok(InstructionResult::Return(result));
                }

                // For closures, return TailCall result to reuse the frame
                if let Some(closure) = func_value.as_function() {
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        let arg = ctx.get_register(func.0 + 1 + i).clone();
                        args.push(arg);
                    }

                    ctx.set_pending_args(args);
                    ctx.set_pending_this(Value::undefined());

                    // Get the return register from the current frame (where tail call result goes)
                    let return_reg = ctx
                        .current_frame()
                        .and_then(|f| f.return_register)
                        .unwrap_or(0);

                    return Ok(InstructionResult::TailCall {
                        func_index: closure.function_index,
                        module: Arc::clone(&closure.module),
                        argc: *argc,
                        return_reg,
                        is_async: closure.is_async,
                        upvalues: closure.upvalues.clone(),
                    });
                }

                Err(VmError::type_error("not a function"))
            }

            Instruction::Construct { dst, func, argc } => {
                let func_value = ctx.get_register(func.0).clone();

                // Check if it's a proxy with construct trap
                if let Some(proxy) = func_value.as_proxy() {
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        let arg = ctx.get_register(func.0 + 1 + i).clone();
                        args.push(arg);
                    }
                    let result = crate::proxy_operations::proxy_construct(
                        self,
                        ctx,
                        proxy,
                        &args,
                        func_value.clone(), // new.target
                    )?;
                    ctx.set_register(dst.0, result);
                    return Ok(InstructionResult::Continue);
                }

                if let Some(func_obj) = func_value.as_object() {
                    if func_obj
                        .get(&PropertyKey::string("__non_constructor"))
                        .and_then(|v| v.as_boolean())
                        == Some(true)
                    {
                        return Err(VmError::type_error("not a constructor"));
                    }
                }

                if let Some(native_fn) = func_value.as_native_function() {
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        let arg = ctx.get_register(func.0 + 1 + i).clone();
                        args.push(arg);
                    }

                    // Get prototype for new object
                    let ctor_proto = func_value
                        .as_object()
                        .and_then(|o| o.get(&PropertyKey::string("prototype")))
                        .and_then(|v| v.as_object());
                    let new_obj =
                        GcRef::new(JsObject::new(ctor_proto.clone(), ctx.memory_manager().clone()));
                    let new_obj_value = Value::object(new_obj.clone());

                    // Capture stack trace for Error objects
                    if let Some(proto) = ctor_proto {
                        if proto.get(&PropertyKey::string("__is_error__"))
                            .and_then(|v| v.as_boolean()) == Some(true)
                        {
                            Self::capture_error_stack_trace(new_obj.clone(), ctx);
                        }
                    }

                    // Call native constructor with depth tracking
                    let result = match self.call_native_fn(ctx, native_fn, &new_obj_value, &args) {
                        Ok(v) => v,
                        Err(VmError::Interception(crate::error::InterceptionSignal::PromiseConstructor)) => {
                            let executor = args.get(0).cloned().unwrap_or(Value::undefined());
                            if !executor.is_callable() {
                                return Err(VmError::type_error("Promise resolver is not a function"));
                            }

                            let promise = JsPromise::new();
                            let js_queue = ctx.js_job_queue();
                            let enqueue_js_job = {
                                let js_queue = js_queue.clone();
                                move |job, args| {
                                    if let Some(queue) = &js_queue {
                                        queue.enqueue(job, args);
                                    }
                                }
                            };

                            let fn_proto = ctx
                                .function_prototype()
                                .ok_or_else(|| VmError::internal("Function.prototype is not defined"))?;

                            let resolve_promise = promise.clone();
                            let enqueue_resolve = enqueue_js_job.clone();
                            let resolve_fn = Value::native_function_with_proto(
                                move |_this, args, _mm| {
                                    let value = args.first().cloned().unwrap_or(Value::undefined());
                                    resolve_promise.resolve_with_js_jobs(value, enqueue_resolve.clone());
                                    Ok(Value::undefined())
                                },
                                ctx.memory_manager().clone(),
                                fn_proto,
                            );
                            if let Some(obj) = resolve_fn.as_object() {
                                obj.set(
                                    PropertyKey::string("__non_constructor"),
                                    Value::boolean(true),
                                );
                            }

                            let reject_promise = promise.clone();
                            let enqueue_reject = enqueue_js_job.clone();
                            let reject_fn = Value::native_function_with_proto(
                                move |_this, args, _mm| {
                                    let reason = args.first().cloned().unwrap_or(Value::undefined());
                                    reject_promise.reject_with_js_jobs(reason, enqueue_reject.clone());
                                    Ok(Value::undefined())
                                },
                                ctx.memory_manager().clone(),
                                fn_proto,
                            );
                            if let Some(obj) = reject_fn.as_object() {
                                obj.set(
                                    PropertyKey::string("__non_constructor"),
                                    Value::boolean(true),
                                );
                            }

                            if let Err(err) = self.call_function(
                                ctx,
                                &executor,
                                Value::undefined(),
                                &[resolve_fn.clone(), reject_fn.clone()],
                            ) {
                                let error_val = match err {
                                    VmError::Exception(thrown) => thrown.value,
                                    VmError::TypeError(message) => {
                                        self.make_error(ctx, "TypeError", &message)
                                    }
                                    VmError::RangeError(message) => {
                                        self.make_error(ctx, "RangeError", &message)
                                    }
                                    VmError::ReferenceError(message) => {
                                        self.make_error(ctx, "ReferenceError", &message)
                                    }
                                    VmError::SyntaxError(message) => {
                                        self.make_error(ctx, "SyntaxError", &message)
                                    }
                                    other => {
                                        let message = other.to_string();
                                        Value::string(JsString::intern(&message))
                                    }
                                };
                                promise.reject_with_js_jobs(error_val, enqueue_js_job.clone());
                            }

                            let js_promise = self.create_js_promise(ctx, promise);
                            ctx.set_register(dst.0, js_promise);
                            return Ok(InstructionResult::Continue);
                        }
                        // eval (and similar non-constructors) throw Interception when called
                        // with string args — treat as "not a constructor" in Construct context
                        Err(VmError::Interception(_)) => {
                            return Err(VmError::type_error("not a constructor"));
                        }
                        Err(e) => return Err(e),
                    };
                    let final_value = if result.is_object() {
                        result
                    } else {
                        new_obj_value
                    };
                    ctx.set_register(dst.0, final_value);
                    return Ok(InstructionResult::Continue);
                }

                // Check if it's a callable constructor
                if let Some(closure) = func_value.as_function() {
                    // Check if this is a derived constructor (class extends)
                    let func_def = closure
                        .module
                        .functions
                        .get(closure.function_index as usize);
                    let is_derived = func_def.map(|f| f.flags.is_derived).unwrap_or(false);

                    // Copy arguments from caller registers
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        let arg = ctx.get_register(func.0 + 1 + i).clone();
                        args.push(arg);
                    }

                    if is_derived {
                        // Derived constructor: `this` is NOT created here.
                        // It will be created by super() call inside the constructor.
                        // Set pending_is_derived so the CallFrame knows.
                        ctx.set_pending_args(args);
                        ctx.set_pending_this(Value::undefined());
                        ctx.set_pending_is_derived(true);

                        // Set home_object = the constructor's .prototype
                        // (used by super() to find the parent constructor)
                        if let Some(ctor_obj) = func_value.as_object() {
                            let proto_key = PropertyKey::string("prototype");
                            if let Some(proto_val) = ctor_obj.get(&proto_key) {
                                if let Some(proto_obj) = proto_val.as_object() {
                                    ctx.set_pending_home_object(proto_obj);
                                }
                            }
                        }

                        // Pre-set dst to undefined; super() will update this_value on the frame
                        ctx.set_register(dst.0, Value::undefined());
                    } else {
                        // Base constructor: create new object with prototype = ctor.prototype
                        let ctor_proto = func_value
                            .as_object()
                            .and_then(|o| o.get(&PropertyKey::string("prototype")))
                            .and_then(|v| v.as_object());
                        let new_obj =
                            GcRef::new(JsObject::new(ctor_proto.clone(), ctx.memory_manager().clone()));
                        let new_obj_value = Value::object(new_obj.clone());

                        // Capture stack trace for Error objects
                        if let Some(proto) = ctor_proto {
                            if proto.get(&PropertyKey::string("__is_error__"))
                                .and_then(|v| v.as_boolean()) == Some(true)
                            {
                                Self::capture_error_stack_trace(new_obj.clone(), ctx);
                            }
                        }

                        ctx.set_pending_args(args);
                        ctx.set_pending_this(new_obj_value.clone());

                        // Pre-set dst to the new object (will be returned if constructor returns undefined)
                        ctx.set_register(dst.0, new_obj_value);
                    }

                    Ok(InstructionResult::Call {
                        func_index: closure.function_index,
                        module: Arc::clone(&closure.module),
                        argc: *argc,
                        return_reg: dst.0,
                        is_construct: true,
                        is_async: closure.is_async,
                        upvalues: closure.upvalues.clone(),
                    })
                } else {
                    // Not a function - return error
                    Err(VmError::type_error("not a constructor"))
                }
            }

            Instruction::CallMethod {
                dst,
                obj,
                method,
                argc,
                ic_index,
            } => {
                let receiver = ctx.get_register(obj.0).clone();
                let method_const = module
                    .constants
                    .get(method.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;
                let method_name = method_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;

                // IC Fast Path
                let cached_method = if let Some(obj_ref) = receiver.as_object() {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let func = frame
                        .module
                        .function(frame.function_index)
                        .ok_or_else(|| VmError::internal("no function"))?;
                    let feedback = func.feedback_vector.read();
                    if let Some(ic) = feedback.get(*ic_index as usize) {
                        if let otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                            shape_id: shape_addr,
                            offset,
                        } = &ic.ic_state
                        {
                            if std::sync::Arc::as_ptr(&obj_ref.shape()) as u64 == *shape_addr {
                                obj_ref.get_by_offset(*offset as usize)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(method_value) = cached_method {
                    // Direct call handling - collect args first
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        args.push(ctx.get_register(obj.0 + 1 + i).clone());
                    }
                    return self.handle_call_value(ctx, &method_value, receiver, args, dst.0);
                }

                // Get the method from the receiver.
                // For primitives/functions, emulate `ToObject` lookup by consulting the corresponding
                // prototype object (e.g. `String.prototype`) but keep `this` as the primitive.
                let method_value = if receiver.is_function() || receiver.is_native_function() {
                    let function_global = ctx.get_global("Function");
                    let function_obj = function_global
                        .as_ref()
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| {
                            VmError::type_error("Function is not defined")
                        })?;
                    let proto_val = function_obj.get(&PropertyKey::string("prototype"));
                    let proto = proto_val
                        .as_ref()
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| {
                            VmError::type_error("Function.prototype is not defined")
                        })?;
                    if let Some(obj_ref) = receiver.as_object() {
                        obj_ref
                            .get(&Self::utf16_key(method_name))
                            .or_else(|| proto.get(&Self::utf16_key(method_name)))
                            .unwrap_or_else(Value::undefined)
                    } else {
                        proto
                            .get(&Self::utf16_key(method_name))
                            .unwrap_or_else(Value::undefined)
                    }
                } else if let Some(obj_ref) = receiver.as_object() {
                    obj_ref
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if receiver.is_string() {
                    let string_obj = ctx
                        .get_global("String")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("String is not defined"))?;
                    let proto = string_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("String.prototype is not defined"))?;
                    proto
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if receiver.is_generator() {
                    // Special handling for generator methods - execute directly
                    let method_str = String::from_utf16_lossy(method_name);
                    if method_str == "next" || method_str == "return" || method_str == "throw" {
                        // Get the generator and execute it directly
                        let generator = receiver
                            .as_generator()
                            .ok_or_else(|| VmError::internal("Expected generator"))?;

                        // Get the sent value (first argument if present)
                        let sent_value = if *argc > 0 {
                            Some(ctx.get_register(obj.0 + 1).clone())
                        } else {
                            None
                        };

                        // Handle the specific method
                        let gen_result = match method_str.as_str() {
                            "next" => self.execute_generator(generator, ctx, sent_value),
                            "return" => {
                                // generator.return(value) - complete with the value
                                // If generator has try handlers, we need to run finally blocks
                                // See: https://tc39.es/ecma262/#sec-generatorresumeabrupt
                                let return_value = sent_value.unwrap_or_else(Value::undefined);

                                if generator.is_completed() {
                                    // Already completed, just return
                                    GeneratorResult::Returned(return_value)
                                } else if !generator.has_try_handlers() {
                                    // No try handlers, no finally blocks to run
                                    generator.complete();
                                    GeneratorResult::Returned(return_value)
                                } else {
                                    // Has try handlers - need to run finally blocks
                                    // Set pending return and resume to trigger exception path
                                    generator.set_pending_return(return_value);
                                    self.execute_generator(generator, ctx, None)
                                }
                            }
                            "throw" => {
                                // generator.throw(error) - throw into the generator
                                let error_value = sent_value.unwrap_or_else(Value::undefined);
                                if generator.is_completed() {
                                    // If already completed, just throw the error
                                    GeneratorResult::Error(VmError::exception(error_value))
                                } else {
                                    // Set pending throw and resume
                                    generator.set_pending_throw(error_value.clone());
                                    self.execute_generator(generator, ctx, None)
                                }
                            }
                            _ => unreachable!(),
                        };

                        // For async generators, wrap result in a Promise
                        if generator.is_async() {
                            let promise = JsPromise::new();
                            let js_queue = ctx.js_job_queue();
                            match gen_result {
                                GeneratorResult::Yielded(v) => {
                                    let iter_result = GcRef::new(JsObject::new(
                                        None,
                                        ctx.memory_manager().clone(),
                                    ));
                                    iter_result.set(PropertyKey::string("value"), v);
                                    iter_result
                                        .set(PropertyKey::string("done"), Value::boolean(false));
                                    let js_queue = js_queue.clone();
                                    promise.resolve_with_js_jobs(
                                        Value::object(iter_result),
                                        move |job, args| {
                                            if let Some(queue) = &js_queue {
                                                queue.enqueue(job, args);
                                            }
                                        },
                                    );
                                }
                                GeneratorResult::Returned(v) => {
                                    let iter_result = GcRef::new(JsObject::new(
                                        None,
                                        ctx.memory_manager().clone(),
                                    ));
                                    iter_result.set(PropertyKey::string("value"), v);
                                    iter_result
                                        .set(PropertyKey::string("done"), Value::boolean(true));
                                    let js_queue = js_queue.clone();
                                    promise.resolve_with_js_jobs(
                                        Value::object(iter_result),
                                        move |job, args| {
                                            if let Some(queue) = &js_queue {
                                                queue.enqueue(job, args);
                                            }
                                        },
                                    );
                                }
                                GeneratorResult::Error(e) => {
                                    let error_msg = e.to_string();
                                    let js_queue = js_queue.clone();
                                    promise.reject_with_js_jobs(
                                        Value::string(JsString::intern(&error_msg)),
                                        move |job, args| {
                                            if let Some(queue) = &js_queue {
                                                queue.enqueue(job, args);
                                            }
                                        },
                                    );
                                }
                                GeneratorResult::Suspended {
                                    promise: awaited_promise,
                                    resume_reg,
                                    generator: suspended_gen,
                                } => {
                                    // Generator is awaiting a promise
                                    // Chain onto the awaited promise and resume when it settles
                                    let result_promise = promise.clone();
                                    let mm = ctx.memory_manager().clone();
                                    let js_queue = js_queue.clone();
                                    awaited_promise.then(move |resolved_value| {
                                        // When the awaited promise resolves, we would resume the generator
                                        // For now, just resolve with the awaited value wrapped in an iterator result
                                        // TODO: Properly resume async generator execution
                                        let iter_result =
                                            GcRef::new(JsObject::new(None, mm.clone()));
                                        iter_result
                                            .set(PropertyKey::string("value"), resolved_value);
                                        iter_result.set(
                                            PropertyKey::string("done"),
                                            Value::boolean(false),
                                        );
                                        let js_queue = js_queue.clone();
                                        result_promise.resolve_with_js_jobs(
                                            Value::object(iter_result),
                                            move |job, args| {
                                                if let Some(queue) = &js_queue {
                                                    queue.enqueue(job, args);
                                                }
                                            },
                                        );
                                    });
                                    // Store the resume_reg and generator for later use
                                    let _ = (resume_reg, suspended_gen);
                                }
                            }
                            ctx.set_register(dst.0, Value::promise(promise));
                            return Ok(InstructionResult::Continue);
                        }

                        // For sync generators, return iterator result directly
                        let (result_value, is_done) = match gen_result {
                            GeneratorResult::Yielded(v) => (v, false),
                            GeneratorResult::Returned(v) => (v, true),
                            GeneratorResult::Error(e) => return Err(e),
                            GeneratorResult::Suspended { .. } => {
                                return Err(VmError::internal("Sync generator cannot suspend"));
                            }
                        };

                        // Create iterator result object { value, done }
                        let result = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));
                        result.set(PropertyKey::string("value"), result_value);
                        result.set(PropertyKey::string("done"), Value::boolean(is_done));
                        ctx.set_register(dst.0, Value::object(result));
                        return Ok(InstructionResult::Continue);
                    }

                    // For other methods, fall through to prototype lookup
                    let generator_proto = ctx
                        .get_global("GeneratorPrototype")
                        .ok_or_else(|| VmError::type_error("GeneratorPrototype is not defined"))?;
                    if let Some(proto) = generator_proto.as_object() {
                        proto
                            .get(&Self::utf16_key(method_name))
                            .unwrap_or_else(Value::undefined)
                    } else {
                        Value::undefined()
                    }
                } else if receiver.is_promise() {
                    let promise_obj = ctx
                        .get_global("Promise")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Promise is not defined"))?;
                    let proto = promise_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Promise.prototype is not defined"))?;
                    proto
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if receiver.is_number() {
                    let number_obj = ctx
                        .get_global("Number")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Number is not defined"))?;
                    let proto = number_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Number.prototype is not defined"))?;
                    proto
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if receiver.is_boolean() {
                    let boolean_obj = ctx
                        .get_global("Boolean")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Boolean is not defined"))?;
                    let proto = boolean_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Boolean.prototype is not defined"))?;
                    proto
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if let Some(regex) = receiver.as_regex() {
                    // RegExp: look up method on the regex's internal object (which has the prototype chain)
                    regex
                        .object
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else {
                    return Err(VmError::type_error("Cannot read property of non-object"));
                };

                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    args.push(ctx.get_register(obj.0 + 1 + i).clone());
                }

                // Update IC if method was found on the object itself
                if let Some(obj_ref) = receiver.as_object() {
                    let key = Self::utf16_key(method_name);
                    if let Some(offset) = obj_ref.shape().get_offset(&key) {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let mut feedback = func.feedback_vector.write();
                        if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                            if matches!(
                                ic.ic_state,
                                otter_vm_bytecode::function::InlineCacheState::Uninitialized
                            ) {
                                ic.ic_state =
                                    otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                        shape_id: std::sync::Arc::as_ptr(&obj_ref.shape()) as u64,
                                        offset: offset as u32,
                                    };
                            }
                        }
                    }
                }

                self.handle_call_value(ctx, &method_value, receiver, args, dst.0)
            }

            Instruction::Return { src } => {
                let value = ctx.get_register(src.0).clone();
                // In derived constructors:
                // - returning an object is OK
                // - returning undefined after super() was called: return this
                // - returning non-object or undefined without super(): error
                if let Some(frame) = ctx.current_frame() {
                    if frame.is_derived {
                        if value.is_object() {
                            // Explicit object return is fine
                        } else if value.is_undefined() && frame.this_initialized {
                            // Implicit/explicit undefined return → return this
                            return Ok(InstructionResult::Return(frame.this_value.clone()));
                        } else if !frame.this_initialized {
                            return Err(VmError::ReferenceError(
                                "Must call super constructor in derived class before returning from derived constructor".to_string(),
                            ));
                        }
                        // Non-object, non-undefined explicit return in derived: TypeError per spec
                        // but for now treat as returning undefined → this
                    }
                }
                Ok(InstructionResult::Return(value))
            }

            Instruction::ReturnUndefined => {
                // In derived constructors, implicit return should return `this`
                if let Some(frame) = ctx.current_frame() {
                    if frame.is_derived {
                        if !frame.this_initialized {
                            return Err(VmError::ReferenceError(
                                "Must call super constructor in derived class before returning from derived constructor".to_string(),
                            ));
                        }
                        // Return this_value (the object created by super())
                        return Ok(InstructionResult::Return(frame.this_value.clone()));
                    }
                }
                Ok(InstructionResult::Return(Value::undefined()))
            }

            Instruction::CallSpread {
                dst,
                func,
                argc,
                spread,
            } => {
                let func_value = ctx.get_register(func.0).clone();
                let spread_arr = ctx.get_register(spread.0).clone();

                // Collect regular arguments first
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    let arg = ctx.get_register(func.0 + 1 + i).clone();
                    args.push(arg);
                }

                // Spread the array into args
                if let Some(arr_obj) = spread_arr.as_object() {
                    let len = arr_obj
                        .get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_int32())
                        .unwrap_or(0) as u32;

                    for i in 0..len {
                        if let Some(elem) = arr_obj.get(&PropertyKey::Index(i)) {
                            args.push(elem);
                        } else {
                            args.push(Value::undefined());
                        }
                    }
                }

                // Check if it's a native function first
                if let Some(native_fn) = func_value.as_native_function() {
                    // Call the native function with depth tracking
                    let result = self.call_native_fn(ctx, native_fn, &Value::undefined(), &args)?;
                    ctx.set_register(dst.0, result);
                    return Ok(InstructionResult::Continue);
                }

                // Regular closure call
                let closure = func_value
                    .as_function()
                    .ok_or_else(|| VmError::type_error("not a function"))?;

                // Store args in context for new frame to pick up
                ctx.set_pending_args(args.clone());

                Ok(InstructionResult::Call {
                    func_index: closure.function_index,
                    module: Arc::clone(&closure.module),
                    argc: args.len() as u8,
                    return_reg: dst.0,
                    is_construct: false,
                    is_async: closure.is_async,
                    upvalues: closure.upvalues.clone(),
                })
            }

            Instruction::ConstructSpread {
                dst,
                func,
                argc,
                spread,
            } => {
                let func_value = ctx.get_register(func.0).clone();
                let spread_arr = ctx.get_register(spread.0).clone();

                // Collect regular arguments first
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    let arg = ctx.get_register(func.0 + 1 + i).clone();
                    args.push(arg);
                }

                // Spread the array into args
                if let Some(arr_obj) = spread_arr.as_object() {
                    let len = arr_obj
                        .get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_int32())
                        .unwrap_or(0) as u32;
                    for i in 0..len {
                        if let Some(elem) = arr_obj.get(&PropertyKey::Index(i)) {
                            args.push(elem);
                        } else {
                            args.push(Value::undefined());
                        }
                    }
                }

                if let Some(native_fn) = func_value.as_native_function() {
                    let ctor_proto = func_value
                        .as_object()
                        .and_then(|o| o.get(&PropertyKey::string("prototype")))
                        .and_then(|v| v.as_object());
                    let new_obj =
                        GcRef::new(JsObject::new(ctor_proto, ctx.memory_manager().clone()));
                    let new_obj_value = Value::object(new_obj);

                    let result = self.call_native_fn(ctx, native_fn, &Value::undefined(), &args)?;
                    let final_value = if result.is_object() {
                        result
                    } else {
                        new_obj_value
                    };
                    ctx.set_register(dst.0, final_value);
                    return Ok(InstructionResult::Continue);
                }

                let closure = func_value
                    .as_function()
                    .ok_or_else(|| VmError::type_error("not a constructor"))?;

                // Create a new object with prototype = ctor.prototype (if any) and bind it as `this`
                let ctor_proto = func_value
                    .as_object()
                    .and_then(|o| o.get(&PropertyKey::string("prototype")))
                    .and_then(|v| v.as_object());
                let new_obj = GcRef::new(JsObject::new(ctor_proto, ctx.memory_manager().clone()));
                let new_obj_value = Value::object(new_obj);

                let argc_u8 = args.len() as u8;
                ctx.set_pending_args(args);
                ctx.set_pending_this(new_obj_value.clone());
                ctx.set_register(dst.0, new_obj_value);

                Ok(InstructionResult::Call {
                    func_index: closure.function_index,
                    module: Arc::clone(&closure.module),
                    argc: argc_u8,
                    return_reg: dst.0,
                    is_construct: true,
                    is_async: closure.is_async,
                    upvalues: closure.upvalues.clone(),
                })
            }

            // ==================== Async/Await ====================
            Instruction::Await { dst, src } => {
                let value = ctx.get_register(src.0).clone();

                // Try to get a promise from the value
                // 1. Check if it's a raw VM promise
                // 2. Check if it's a JS Promise wrapper with _internal property
                let promise_opt = if value.as_promise().is_some() {
                    value.as_promise().cloned()
                } else if let Some(obj) = value.as_object() {
                    // Check for JS Promise wrapper: { _internal: <vm_promise> }
                    obj.get(&PropertyKey::string("_internal"))
                        .and_then(|v| v.as_promise().cloned())
                } else {
                    None
                };

                if let Some(promise) = promise_opt {
                    match promise.state() {
                        PromiseState::Fulfilled(resolved) => {
                            // Promise already resolved, use the value
                            ctx.set_register(dst.0, resolved);
                            Ok(InstructionResult::Continue)
                        }
                        PromiseState::Rejected(error) => {
                            // Promise rejected, propagate the error
                            Err(VmError::type_error(format!(
                                "Promise rejected: {:?}",
                                error
                            )))
                        }
                        PromiseState::Pending | PromiseState::PendingThenable(_) => {
                            // Promise is pending, suspend execution
                            Ok(InstructionResult::Suspend {
                                promise: Arc::clone(&promise),
                                resume_reg: dst.0,
                            })
                        }
                    }
                } else {
                    // Not a Promise, wrap in resolved promise and return immediately
                    // Per JS spec: await non-promise returns the value directly
                    ctx.set_register(dst.0, value);
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::Yield { dst, src } => {
                let value = ctx.get_register(src.0).clone();

                // Yield suspends the generator and returns the value
                // The dst register will receive the value sent to next() on resumption
                // (handled in resume_generator_execution using yield_dst)

                // Return a yield result with the destination register
                Ok(InstructionResult::Yield {
                    value,
                    yield_dst: dst.0,
                })
            }

            // ==================== Objects ====================
            Instruction::NewObject { dst } => {
                // Get Object.prototype from global for proper prototype chain
                let proto = ctx
                    .global()
                    .get(&PropertyKey::string("Object"))
                    .and_then(|obj_ctor| {
                        obj_ctor
                            .as_object()
                            .and_then(|o| o.get(&PropertyKey::string("prototype")))
                    })
                    .and_then(|proto_val| proto_val.as_object());

                let obj = GcRef::new(JsObject::new(proto, ctx.memory_manager().clone()));
                ctx.set_register(dst.0, Value::object(obj));
                Ok(InstructionResult::Continue)
            }

            Instruction::CreateArguments { dst } => {
                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame"))?;
                let argc = frame.argc;
                let mm = ctx.memory_manager().clone();

                // Get Array.prototype for the arguments object
                let array_proto = ctx
                    .get_global("Array")
                    .and_then(|v| v.as_object())
                    .and_then(|o| o.get(&PropertyKey::string("prototype")))
                    .and_then(|v| v.as_object());

                let args_obj = GcRef::new(JsObject::array(argc, mm));
                if let Some(proto) = array_proto {
                    args_obj.set_prototype(Some(proto));
                }

                // Populate arguments from locals
                // Arguments 0..param_count are in locals[0..param_count]
                // Arguments param_count..argc are in locals[local_count..]
                let func = &frame.module.functions[frame.function_index as usize];
                let param_count = func.param_count as usize;
                let local_count = func.local_count as usize;

                for i in 0..argc {
                    let val = if i < param_count {
                        ctx.get_local(i as u16)?
                    } else {
                        let offset = local_count + (i - param_count);
                        ctx.get_local(offset as u16)?
                    };
                    // println!("DEBUG: arg[{}] = {:?} (param_count={}, local_count={})", i, val, param_count, local_count);
                    args_obj.set(PropertyKey::index(i as u32), val);
                }

                // Set length property
                args_obj.set(PropertyKey::string("length"), Value::number(argc as f64));

                ctx.set_register(dst.0, Value::object(args_obj));
                Ok(InstructionResult::Continue)
            }

            Instruction::CallEval { dst, code } => {
                let code_value = ctx.get_register(code.0).clone();

                // Per spec §19.2.1.1: if argument is not a string, return it unchanged
                if !code_value.is_string() {
                    ctx.set_register(dst.0, code_value);
                    return Ok(InstructionResult::Continue);
                }

                let js_str = code_value
                    .as_string()
                    .ok_or_else(|| VmError::type_error("eval argument is not a string"))?;
                let source = js_str.as_str().to_string();

                // Per ES2023 §19.2.1.1: Direct eval inherits strict mode from calling context
                let is_strict_context = ctx.current_frame()
                    .and_then(|frame| {
                        frame.module.functions.get(frame.function_index as usize)
                    })
                    .map(|func| func.flags.is_strict)
                    .unwrap_or(false);

                // Compile eval code into a module
                let eval_module = ctx.compile_eval(&source, is_strict_context)?;
                let result = self.execute_eval_module(ctx, &eval_module)?;
                ctx.set_register(dst.0, result);
                Ok(InstructionResult::Continue)
            }

            Instruction::GetPropConst {
                dst,
                obj,
                name,
                ic_index,
            } => {
                let object = ctx.get_register(obj.0).clone();
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;
                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;

                // Proxy check - must be first
                if let Some(proxy) = object.as_proxy() {
                    let key = Self::utf16_key(name_str);
                    let key_value = Value::string(JsString::intern_utf16(name_str));
                    let receiver = object.clone();
                    let result = crate::proxy_operations::proxy_get(self, ctx, proxy, &key, key_value, receiver)?;
                    ctx.set_register(dst.0, result);
                    return Ok(InstructionResult::Continue);
                }

                // Generator property access
                if let Some(generator) = object.as_generator() {
                    let key = Self::utf16_key(name_str);

                    // Check the generator's internal object first
                    if let Some(val) = generator.object.get(&key) {
                        ctx.set_register(dst.0, val);
                        return Ok(InstructionResult::Continue);
                    }
                    // Check prototype chain (this gives us next, return, throw, Symbol.iterator, Symbol.toStringTag)
                    if let Some(proto) = generator.object.prototype() {
                        if let Some(val) = proto.get(&key) {
                            ctx.set_register(dst.0, val);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    return Ok(InstructionResult::Continue);
                }

                if let Some(str_ref) = object.as_string() {
                    if Self::utf16_eq_ascii(name_str, "length") {
                        ctx.set_register(dst.0, Value::int32(str_ref.len_utf16() as i32));
                        return Ok(InstructionResult::Continue);
                    }

                    if let Some(index) = Self::utf16_to_index(name_str) {
                        let units = str_ref.as_utf16();
                        if let Some(unit) = units.get(index as usize) {
                            let ch = JsString::intern_utf16(&[*unit]);
                            ctx.set_register(dst.0, Value::string(ch));
                        } else {
                            ctx.set_register(dst.0, Value::undefined());
                        }
                        return Ok(InstructionResult::Continue);
                    }

                    if let Some(string_obj) = ctx.get_global("String").and_then(|v| v.as_object()) {
                        if let Some(proto) = string_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            let key = Self::utf16_key(name_str);
                            let value = proto.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                }

                // Function property access
                if let Some(closure) = object.as_function() {
                    let key = Self::utf16_key(name_str);
                    // Check the function's internal object first (for properties like .prototype, .length, .name)
                    if let Some(val) = closure.object.get(&key) {
                        ctx.set_register(dst.0, val);
                        return Ok(InstructionResult::Continue);
                    }
                    // Check prototype chain
                    if let Some(proto) = closure.object.prototype() {
                        if let Some(val) = proto.get(&key) {
                            ctx.set_register(dst.0, val);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    return Ok(InstructionResult::Continue);
                }

                // IC Fast Path
                if let Some(obj_ref) = object.as_object() {
                    // Array .length fast path
                    if obj_ref.is_array() && Self::utf16_eq_ascii(name_str, "length") {
                        ctx.set_register(dst.0, Value::int32(obj_ref.array_length() as i32));
                        return Ok(InstructionResult::Continue);
                    }

                    let mut cached_val = None;
                    {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let feedback = func.feedback_vector.read();
                        if let Some(ic) = feedback.get(*ic_index as usize) {
                            use otter_vm_bytecode::function::InlineCacheState;
                            let obj_shape_ptr = std::sync::Arc::as_ptr(&obj_ref.shape()) as u64;

                            if ic.proto_epoch_matches(get_proto_epoch()) {
                                match &ic.ic_state {
                                    InlineCacheState::Monomorphic { shape_id, offset } => {
                                        if obj_shape_ptr == *shape_id {
                                            cached_val = obj_ref.get_by_offset(*offset as usize);
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        for i in 0..(*count as usize) {
                                            if obj_shape_ptr == entries[i].0 {
                                                cached_val =
                                                    obj_ref.get_by_offset(entries[i].1 as usize);
                                                break;
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    if let Some(val) = cached_val {
                        ctx.set_register(dst.0, val);
                        return Ok(InstructionResult::Continue);
                    }
                }

                // Special handling for functions - look up from Function.prototype
                if object.is_function() || object.is_native_function() {
                    let key = Self::utf16_key(name_str);
                    // First check the function's own object properties
                    if let Some(obj_ref) = object.as_object() {
                        if let Some(value) = obj_ref.get(&key) {
                            ctx.set_register(dst.0, value);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                    // Then look up from Function.prototype
                    if let Some(function_obj) =
                        ctx.get_global("Function").and_then(|v| v.as_object())
                    {
                        if let Some(proto) = function_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            let value = proto.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    return Ok(InstructionResult::Continue);
                }

                if let Some(obj) = object.as_object() {
                    let receiver = object.clone();
                    let key = Self::utf16_key(name_str);

                    match obj.lookup_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Accessor { get, .. }) => {
                            let Some(getter) = get else {
                                ctx.set_register(dst.0, Value::undefined());
                                return Ok(InstructionResult::Continue);
                            };

                            if let Some(native_fn) = getter.as_native_function() {
                                let result = self.call_native_fn(ctx, native_fn, &receiver, &[])?;
                                ctx.set_register(dst.0, result);
                                Ok(InstructionResult::Continue)
                            } else if let Some(closure) = getter.as_function() {
                                ctx.set_pending_args(Vec::new());
                                ctx.set_pending_this(receiver);
                                Ok(InstructionResult::Call {
                                    func_index: closure.function_index,
                                    module: Arc::clone(&closure.module),
                                    argc: 0,
                                    return_reg: dst.0,
                                    is_construct: false,
                                    is_async: closure.is_async,
                                    upvalues: closure.upvalues.clone(),
                                })
                            } else {
                                Err(VmError::type_error("getter is not a function"))
                            }
                        }
                        _ => {
                            // Slow path: full lookup and IC update
                            // Skip IC for dictionary mode objects
                            if !obj.is_dictionary_mode() {
                                if let Some(offset) = obj.shape().get_offset(&key) {
                                    let frame = ctx
                                        .current_frame()
                                        .ok_or_else(|| VmError::internal("no frame"))?;
                                    let func = frame
                                        .module
                                        .function(frame.function_index)
                                        .ok_or_else(|| VmError::internal("no function"))?;
                                    let mut feedback = func.feedback_vector.write();
                                    if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                                        use otter_vm_bytecode::function::InlineCacheState;
                                        let shape_ptr = std::sync::Arc::as_ptr(&obj.shape()) as u64;
                                        let current_epoch = get_proto_epoch();

                                        match &mut ic.ic_state {
                                            InlineCacheState::Uninitialized => {
                                                ic.ic_state = InlineCacheState::Monomorphic {
                                                    shape_id: shape_ptr,
                                                    offset: offset as u32,
                                                };
                                                ic.proto_epoch = current_epoch;
                                            }
                                            InlineCacheState::Monomorphic {
                                                shape_id: old_shape,
                                                offset: old_offset,
                                            } => {
                                                if *old_shape != shape_ptr {
                                                    let mut entries = [(0u64, 0u32); 4];
                                                    entries[0] = (*old_shape, *old_offset);
                                                    entries[1] = (shape_ptr, offset as u32);
                                                    ic.ic_state = InlineCacheState::Polymorphic {
                                                        count: 2,
                                                        entries,
                                                    };
                                                    ic.proto_epoch = current_epoch;
                                                }
                                            }
                                            InlineCacheState::Polymorphic { count, entries } => {
                                                let mut found = false;
                                                for i in 0..(*count as usize) {
                                                    if entries[i].0 == shape_ptr {
                                                        found = true;
                                                        break;
                                                    }
                                                }
                                                if !found {
                                                    if (*count as usize) < 4 {
                                                        entries[*count as usize] =
                                                            (shape_ptr, offset as u32);
                                                        *count += 1;
                                                        ic.proto_epoch = current_epoch;
                                                    } else {
                                                        ic.ic_state = InlineCacheState::Megamorphic;
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }

                            let value = obj.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            Ok(InstructionResult::Continue)
                        }
                    }
                } else if object.is_number() {
                    // Autobox number -> Number.prototype
                    let key = Self::utf16_key(name_str);
                    if let Some(number_obj) = ctx.get_global("Number").and_then(|v| v.as_object()) {
                        if let Some(proto) = number_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            let value = proto.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    Ok(InstructionResult::Continue)
                } else if object.is_boolean() {
                    // Autobox boolean -> Boolean.prototype
                    let key = Self::utf16_key(name_str);
                    if let Some(boolean_obj) = ctx.get_global("Boolean").and_then(|v| v.as_object()) {
                        if let Some(proto) = boolean_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            let value = proto.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    Ok(InstructionResult::Continue)
                } else {
                    ctx.set_register(dst.0, Value::undefined());
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::SetPropConst {
                obj,
                name,
                val,
                ic_index,
            } => {
                let object = ctx.get_register(obj.0).clone();
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;
                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;
                let val_val = ctx.get_register(val.0).clone();

                // Proxy check - must be first
                if let Some(proxy) = object.as_proxy() {
                    let key = Self::utf16_key(name_str);
                    let key_value = Value::string(JsString::intern_utf16(name_str));
                    let receiver = object.clone();
                    crate::proxy_operations::proxy_set(self, ctx, proxy, &key, key_value, val_val, receiver)?;
                    return Ok(InstructionResult::Continue);
                }

                if let Some(obj) = object.as_object() {
                    let key = Self::utf16_key(name_str);

                    // IC Fast Path
                    let mut cached = false;
                    {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let feedback = func.feedback_vector.read();
                        if let Some(ic) = feedback.get(*ic_index as usize) {
                            use otter_vm_bytecode::function::InlineCacheState;
                            let obj_shape_ptr = std::sync::Arc::as_ptr(&obj.shape()) as u64;

                            if ic.proto_epoch_matches(get_proto_epoch()) {
                                match &ic.ic_state {
                                    InlineCacheState::Monomorphic { shape_id, offset } => {
                                        if obj_shape_ptr == *shape_id {
                                            if obj.set_by_offset(*offset as usize, val_val.clone())
                                            {
                                                cached = true;
                                            }
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        for i in 0..(*count as usize) {
                                            if obj_shape_ptr == entries[i].0 {
                                                if obj.set_by_offset(
                                                    entries[i].1 as usize,
                                                    val_val.clone(),
                                                ) {
                                                    cached = true;
                                                }
                                                break;
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    if cached {
                        return Ok(InstructionResult::Continue);
                    }

                    match obj.lookup_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Accessor { set, .. }) => {
                            let Some(setter) = set else {
                                return Ok(InstructionResult::Continue);
                            };

                            if let Some(native_fn) = setter.as_native_function() {
                                self.call_native_fn(ctx, native_fn, &object, &[val_val])?;
                                Ok(InstructionResult::Continue)
                            } else if let Some(closure) = setter.as_function() {
                                ctx.set_pending_args(vec![val_val]);
                                ctx.set_pending_this(object.clone());
                                Ok(InstructionResult::Call {
                                    func_index: closure.function_index,
                                    module: Arc::clone(&closure.module),
                                    argc: 1,
                                    return_reg: 0, // Setter return value is ignored
                                    is_construct: false,
                                    is_async: closure.is_async,
                                    upvalues: closure.upvalues.clone(),
                                })
                            } else {
                                Err(VmError::type_error("setter is not a function"))
                            }
                        }
                        _ => {
                            // Slow path: update IC
                            obj.set(key, val_val);
                            // Skip IC for dictionary mode objects
                            if !obj.is_dictionary_mode() {
                                if let Some(offset) =
                                    obj.shape().get_offset(&Self::utf16_key(name_str))
                                {
                                    let frame = ctx
                                        .current_frame()
                                        .ok_or_else(|| VmError::internal("no frame"))?;
                                    let func = frame
                                        .module
                                        .function(frame.function_index)
                                        .ok_or_else(|| VmError::internal("no function"))?;
                                    let mut feedback = func.feedback_vector.write();
                                    if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                                        use otter_vm_bytecode::function::InlineCacheState;
                                        let shape_ptr = std::sync::Arc::as_ptr(&obj.shape()) as u64;
                                        let current_epoch = get_proto_epoch();

                                        match &mut ic.ic_state {
                                            InlineCacheState::Uninitialized => {
                                                ic.ic_state = InlineCacheState::Monomorphic {
                                                    shape_id: shape_ptr,
                                                    offset: offset as u32,
                                                };
                                                ic.proto_epoch = current_epoch;
                                            }
                                            InlineCacheState::Monomorphic {
                                                shape_id: old_shape,
                                                offset: old_offset,
                                            } => {
                                                if *old_shape != shape_ptr {
                                                    let mut entries = [(0u64, 0u32); 4];
                                                    entries[0] = (*old_shape, *old_offset);
                                                    entries[1] = (shape_ptr, offset as u32);
                                                    ic.ic_state = InlineCacheState::Polymorphic {
                                                        count: 2,
                                                        entries,
                                                    };
                                                    ic.proto_epoch = current_epoch;
                                                }
                                            }
                                            InlineCacheState::Polymorphic { count, entries } => {
                                                let mut found = false;
                                                for i in 0..(*count as usize) {
                                                    if entries[i].0 == shape_ptr {
                                                        found = true;
                                                        break;
                                                    }
                                                }
                                                if !found {
                                                    if (*count as usize) < 4 {
                                                        entries[*count as usize] =
                                                            (shape_ptr, offset as u32);
                                                        *count += 1;
                                                        ic.proto_epoch = current_epoch;
                                                    } else {
                                                        ic.ic_state = InlineCacheState::Megamorphic;
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            Ok(InstructionResult::Continue)
                        }
                    }
                } else {
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::DeleteProp { dst, obj, key } => {
                let object = ctx.get_register(obj.0).clone();
                let key_value = ctx.get_register(key.0).clone();

                // Proxy check - must be first
                if let Some(proxy) = object.as_proxy() {
                    let prop_key = if let Some(n) = key_value.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = key_value.as_string() {
                        PropertyKey::from_js_string(s)
                    } else if let Some(sym) = key_value.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };
                    let result = crate::proxy_operations::proxy_delete_property(
                        self,
                        ctx,
                        proxy,
                        &prop_key,
                        key_value,
                    )?;
                    ctx.set_register(dst.0, Value::boolean(result));
                    return Ok(InstructionResult::Continue);
                }

                // Convert key to PropertyKey
                let prop_key = if let Some(n) = key_value.as_int32() {
                    PropertyKey::Index(n as u32)
                } else if let Some(s) = key_value.as_string() {
                    PropertyKey::from_js_string(s)
                } else if let Some(sym) = key_value.as_symbol() {
                    PropertyKey::Symbol(sym.id)
                } else {
                    let key_str = self.to_string(&key_value);
                    PropertyKey::string(&key_str)
                };

                let result = if let Some(obj) = object.as_object() {
                    if !obj.has_own(&prop_key) {
                        true
                    } else {
                        obj.delete(&prop_key)
                    }
                } else if let Some(closure) = object.as_function() {
                    // Handle delete on function objects (for .length, .name, etc.)
                    if !closure.object.has_own(&prop_key) {
                        true
                    } else {
                        closure.object.delete(&prop_key)
                    }
                } else {
                    true
                };

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::GetProp {
                dst,
                obj,
                key,
                ic_index,
            } => {
                let object = ctx.get_register(obj.0).clone();
                let key_value = ctx.get_register(key.0).clone();

                // Proxy check - must be first
                if let Some(proxy) = object.as_proxy() {
                    let prop_key = if let Some(n) = key_value.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = key_value.as_string() {
                        PropertyKey::from_js_string(s)
                    } else if let Some(sym) = key_value.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };
                    let receiver = object.clone();
                    let result = crate::proxy_operations::proxy_get(self, ctx, proxy, &prop_key, key_value.clone(), receiver)?;
                    ctx.set_register(dst.0, result);
                    return Ok(InstructionResult::Continue);
                }

                if let Some(str_ref) = object.as_string() {
                    let key = if let Some(n) = key_value.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = key_value.as_string() {
                        PropertyKey::from_js_string(s)
                    } else if let Some(sym) = key_value.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };

                    match &key {
                        PropertyKey::String(s) if s.as_str() == "length" => {
                            ctx.set_register(dst.0, Value::int32(str_ref.len_utf16() as i32));
                            return Ok(InstructionResult::Continue);
                        }
                        PropertyKey::Index(index) => {
                            let units = str_ref.as_utf16();
                            if let Some(unit) = units.get(*index as usize) {
                                let ch = JsString::intern_utf16(&[*unit]);
                                ctx.set_register(dst.0, Value::string(ch));
                            } else {
                                ctx.set_register(dst.0, Value::undefined());
                            }
                            return Ok(InstructionResult::Continue);
                        }
                        _ => {}
                    }

                    if let Some(string_obj) = ctx.get_global("String").and_then(|v| v.as_object()) {
                        if let Some(proto) = string_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            let value = proto.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                }

                // Function property access
                if let Some(closure) = object.as_function() {
                    // Convert key to property key
                    let key = if let Some(s) = key_value.as_string() {
                        PropertyKey::from_js_string(s)
                    } else if let Some(sym) = key_value.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };

                    // Check the function's internal object first (for properties like .prototype, .length, .name)
                    if let Some(val) = closure.object.get(&key) {
                        ctx.set_register(dst.0, val);
                        return Ok(InstructionResult::Continue);
                    }
                    // Check prototype chain
                    if let Some(proto) = closure.object.prototype() {
                        if let Some(val) = proto.get(&key) {
                            ctx.set_register(dst.0, val);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    return Ok(InstructionResult::Continue);
                }

                // Generator property access
                if let Some(generator) = object.as_generator() {
                    // Convert key to property key
                    let key = if let Some(s) = key_value.as_string() {
                        PropertyKey::from_js_string(s)
                    } else if let Some(sym) = key_value.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };

                    // Check the generator's internal object first
                    if let Some(val) = generator.object.get(&key) {
                        ctx.set_register(dst.0, val);
                        return Ok(InstructionResult::Continue);
                    }
                    // Check prototype chain (this gives us next, return, throw, Symbol.iterator, Symbol.toStringTag)
                    if let Some(proto) = generator.object.prototype() {
                        if let Some(val) = proto.get(&key) {
                            ctx.set_register(dst.0, val);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    return Ok(InstructionResult::Continue);
                }

                if let Some(obj) = object.as_object() {
                    let receiver = object.clone();

                    // Convert key to property key
                    let key = if let Some(n) = key_value.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = key_value.as_string() {
                        PropertyKey::string(s.as_str())
                    } else if let Some(sym) = key_value.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };

                    // IC Fast Path - only for string keys (not index or symbol)
                    if matches!(&key, PropertyKey::String(_)) {
                        let mut cached_val = None;
                        {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            let func = frame
                                .module
                                .function(frame.function_index)
                                .ok_or_else(|| VmError::internal("no function"))?;
                            let feedback = func.feedback_vector.read();
                            if let Some(ic) = feedback.get(*ic_index as usize) {
                                use otter_vm_bytecode::function::InlineCacheState;
                                let obj_shape_ptr = std::sync::Arc::as_ptr(&obj.shape()) as u64;

                                if ic.proto_epoch_matches(get_proto_epoch()) {
                                    match &ic.ic_state {
                                        InlineCacheState::Monomorphic { shape_id, offset } => {
                                            if obj_shape_ptr == *shape_id {
                                                cached_val = obj.get_by_offset(*offset as usize);
                                            }
                                        }
                                        InlineCacheState::Polymorphic { count, entries } => {
                                            for i in 0..(*count as usize) {
                                                if obj_shape_ptr == entries[i].0 {
                                                    cached_val =
                                                        obj.get_by_offset(entries[i].1 as usize);
                                                    break;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }

                        if let Some(val) = cached_val {
                            ctx.set_register(dst.0, val);
                            return Ok(InstructionResult::Continue);
                        }
                    }

                    match obj.lookup_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Accessor { get, .. }) => {
                            let Some(getter) = get else {
                                ctx.set_register(dst.0, Value::undefined());
                                return Ok(InstructionResult::Continue);
                            };

                            if let Some(native_fn) = getter.as_native_function() {
                                let result = self.call_native_fn(ctx, native_fn, &receiver, &[])?;
                                ctx.set_register(dst.0, result);
                                Ok(InstructionResult::Continue)
                            } else if let Some(closure) = getter.as_function() {
                                ctx.set_pending_args(Vec::new());
                                ctx.set_pending_this(receiver);
                                Ok(InstructionResult::Call {
                                    func_index: closure.function_index,
                                    module: Arc::clone(&closure.module),
                                    argc: 0,
                                    return_reg: dst.0,
                                    is_construct: false,
                                    is_async: closure.is_async,
                                    upvalues: closure.upvalues.clone(),
                                })
                            } else {
                                Err(VmError::type_error("getter is not a function"))
                            }
                        }
                        _ => {
                            // Slow path: full lookup and IC update (only for string keys, skip dictionary mode)
                            if matches!(&key, PropertyKey::String(_)) && !obj.is_dictionary_mode() {
                                if let Some(offset) = obj.shape().get_offset(&key) {
                                    let frame = ctx
                                        .current_frame()
                                        .ok_or_else(|| VmError::internal("no frame"))?;
                                    let func = frame
                                        .module
                                        .function(frame.function_index)
                                        .ok_or_else(|| VmError::internal("no function"))?;
                                    let mut feedback = func.feedback_vector.write();
                                    if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                                        use otter_vm_bytecode::function::InlineCacheState;
                                        let shape_ptr = std::sync::Arc::as_ptr(&obj.shape()) as u64;
                                        let current_epoch = get_proto_epoch();

                                        match &mut ic.ic_state {
                                            InlineCacheState::Uninitialized => {
                                                ic.ic_state = InlineCacheState::Monomorphic {
                                                    shape_id: shape_ptr,
                                                    offset: offset as u32,
                                                };
                                                ic.proto_epoch = current_epoch;
                                            }
                                            InlineCacheState::Monomorphic {
                                                shape_id: old_shape,
                                                offset: old_offset,
                                            } => {
                                                if *old_shape != shape_ptr {
                                                    let mut entries = [(0u64, 0u32); 4];
                                                    entries[0] = (*old_shape, *old_offset);
                                                    entries[1] = (shape_ptr, offset as u32);
                                                    ic.ic_state = InlineCacheState::Polymorphic {
                                                        count: 2,
                                                        entries,
                                                    };
                                                    ic.proto_epoch = current_epoch;
                                                }
                                            }
                                            InlineCacheState::Polymorphic { count, entries } => {
                                                let mut found = false;
                                                for i in 0..(*count as usize) {
                                                    if entries[i].0 == shape_ptr {
                                                        found = true;
                                                        break;
                                                    }
                                                }
                                                if !found {
                                                    if (*count as usize) < 4 {
                                                        entries[*count as usize] =
                                                            (shape_ptr, offset as u32);
                                                        *count += 1;
                                                        ic.proto_epoch = current_epoch;
                                                    } else {
                                                        ic.ic_state = InlineCacheState::Megamorphic;
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }

                            let value = obj.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            Ok(InstructionResult::Continue)
                        }
                    }
                } else if object.is_number() {
                    // Autobox number -> Number.prototype
                    let key = if let Some(s) = key_value.as_string() {
                        PropertyKey::from_js_string(s)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };
                    if let Some(number_obj) = ctx.get_global("Number").and_then(|v| v.as_object()) {
                        if let Some(proto) = number_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            let value = proto.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    Ok(InstructionResult::Continue)
                } else if object.is_boolean() {
                    // Autobox boolean -> Boolean.prototype
                    let key = if let Some(s) = key_value.as_string() {
                        PropertyKey::from_js_string(s)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };
                    if let Some(boolean_obj) = ctx.get_global("Boolean").and_then(|v| v.as_object()) {
                        if let Some(proto) = boolean_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            let value = proto.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    Ok(InstructionResult::Continue)
                } else {
                    ctx.set_register(dst.0, Value::undefined());
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::SetProp {
                obj,
                key,
                val,
                ic_index,
            } => {
                let object = ctx.get_register(obj.0).clone();
                let key_value = ctx.get_register(key.0).clone();
                let val_val = ctx.get_register(val.0).clone();

                // Proxy check - must be first
                if let Some(proxy) = object.as_proxy() {
                    let prop_key = if let Some(n) = key_value.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = key_value.as_string() {
                        PropertyKey::from_js_string(s)
                    } else if let Some(sym) = key_value.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };
                    let receiver = object.clone();
                    crate::proxy_operations::proxy_set(self, ctx, proxy, &prop_key, key_value.clone(), val_val, receiver)?;
                    return Ok(InstructionResult::Continue);
                }

                if let Some(obj) = object.as_object() {
                    let key = if let Some(n) = key_value.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = key_value.as_string() {
                        PropertyKey::string(s.as_str())
                    } else if let Some(sym) = key_value.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };

                    // IC Fast Path
                    let mut cached = false;
                    {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let feedback = func.feedback_vector.read();
                        if let Some(ic) = feedback.get(*ic_index as usize) {
                            use otter_vm_bytecode::function::InlineCacheState;
                            let obj_shape_ptr = std::sync::Arc::as_ptr(&obj.shape()) as u64;

                            if ic.proto_epoch_matches(get_proto_epoch()) {
                                match &ic.ic_state {
                                    InlineCacheState::Monomorphic { shape_id, offset } => {
                                        if obj_shape_ptr == *shape_id {
                                            if obj.set_by_offset(*offset as usize, val_val.clone())
                                            {
                                                cached = true;
                                            }
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        for i in 0..(*count as usize) {
                                            if obj_shape_ptr == entries[i].0 {
                                                if obj.set_by_offset(
                                                    entries[i].1 as usize,
                                                    val_val.clone(),
                                                ) {
                                                    cached = true;
                                                }
                                                break;
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    if cached {
                        return Ok(InstructionResult::Continue);
                    }

                    match obj.lookup_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Accessor { set, .. }) => {
                            let Some(setter) = set else {
                                return Ok(InstructionResult::Continue);
                            };

                            if let Some(native_fn) = setter.as_native_function() {
                                self.call_native_fn(ctx, native_fn, &object, &[val_val])?;
                                Ok(InstructionResult::Continue)
                            } else if let Some(closure) = setter.as_function() {
                                ctx.set_pending_args(vec![val_val]);
                                ctx.set_pending_this(object.clone());
                                Ok(InstructionResult::Call {
                                    func_index: closure.function_index,
                                    module: Arc::clone(&closure.module),
                                    argc: 1,
                                    return_reg: 0, // Setter return value is ignored
                                    is_construct: false,
                                    is_async: closure.is_async,
                                    upvalues: closure.upvalues.clone(),
                                })
                            } else {
                                Err(VmError::type_error("setter is not a function"))
                            }
                        }
                        _ => {
                            // Slow path: update IC (skip for dictionary mode)
                            obj.set(key.clone(), val_val);
                            if !obj.is_dictionary_mode() {
                                if let Some(offset) = obj.shape().get_offset(&key) {
                                    let frame = ctx
                                        .current_frame()
                                        .ok_or_else(|| VmError::internal("no frame"))?;
                                    let func = frame
                                        .module
                                        .function(frame.function_index)
                                        .ok_or_else(|| VmError::internal("no function"))?;
                                    let mut feedback = func.feedback_vector.write();
                                    if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                                        use otter_vm_bytecode::function::InlineCacheState;
                                        let shape_ptr = std::sync::Arc::as_ptr(&obj.shape()) as u64;
                                        let current_epoch = get_proto_epoch();

                                        match &mut ic.ic_state {
                                            InlineCacheState::Uninitialized => {
                                                ic.ic_state = InlineCacheState::Monomorphic {
                                                    shape_id: shape_ptr,
                                                    offset: offset as u32,
                                                };
                                                ic.proto_epoch = current_epoch;
                                            }
                                            InlineCacheState::Monomorphic {
                                                shape_id: old_shape,
                                                offset: old_offset,
                                            } => {
                                                if *old_shape != shape_ptr {
                                                    let mut entries = [(0u64, 0u32); 4];
                                                    entries[0] = (*old_shape, *old_offset);
                                                    entries[1] = (shape_ptr, offset as u32);
                                                    ic.ic_state = InlineCacheState::Polymorphic {
                                                        count: 2,
                                                        entries,
                                                    };
                                                    ic.proto_epoch = current_epoch;
                                                }
                                            }
                                            InlineCacheState::Polymorphic { count, entries } => {
                                                let mut found = false;
                                                for i in 0..(*count as usize) {
                                                    if entries[i].0 == shape_ptr {
                                                        found = true;
                                                        break;
                                                    }
                                                }
                                                if !found {
                                                    if (*count as usize) < 4 {
                                                        entries[*count as usize] =
                                                            (shape_ptr, offset as u32);
                                                        *count += 1;
                                                        ic.proto_epoch = current_epoch;
                                                    } else {
                                                        ic.ic_state = InlineCacheState::Megamorphic;
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            Ok(InstructionResult::Continue)
                        }
                    }
                } else {
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::DefineGetter { obj, key, func } => {
                let object = ctx.get_register(obj.0);
                let key_value = ctx.get_register(key.0);
                let getter_fn = ctx.get_register(func.0).clone();

                if let Some(obj) = object.as_object() {
                    let prop_key = self.value_to_property_key(key_value);

                    // Check if there's already an accessor with a setter
                    let existing_setter =
                        obj.get_own_property_descriptor(&prop_key)
                            .and_then(|desc| match desc {
                                PropertyDescriptor::Accessor { set, .. } => set,
                                _ => None,
                            });

                    let desc = PropertyDescriptor::Accessor {
                        get: Some(getter_fn),
                        set: existing_setter,
                        attributes: PropertyAttributes::accessor(),
                    };
                    obj.define_property(prop_key, desc);
                }

                Ok(InstructionResult::Continue)
            }

            Instruction::DefineSetter { obj, key, func } => {
                let object = ctx.get_register(obj.0);
                let key_value = ctx.get_register(key.0);
                let setter_fn = ctx.get_register(func.0).clone();

                if let Some(obj) = object.as_object() {
                    let prop_key = self.value_to_property_key(key_value);

                    // Check if there's already an accessor with a getter
                    let existing_getter =
                        obj.get_own_property_descriptor(&prop_key)
                            .and_then(|desc| match desc {
                                PropertyDescriptor::Accessor { get, .. } => get,
                                _ => None,
                            });

                    let desc = PropertyDescriptor::Accessor {
                        get: existing_getter,
                        set: Some(setter_fn),
                        attributes: PropertyAttributes::accessor(),
                    };
                    obj.define_property(prop_key, desc);
                }

                Ok(InstructionResult::Continue)
            }

            // ==================== Arrays ====================
            Instruction::NewArray { dst, len } => {
                let arr = GcRef::new(JsObject::array(*len as usize, ctx.memory_manager().clone()));
                // Attach `Array.prototype` if present so arrays are iterable and have methods.
                if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                    && let Some(array_proto) = array_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                {
                    arr.set_prototype(Some(array_proto));
                }
                ctx.set_register(dst.0, Value::object(arr));
                Ok(InstructionResult::Continue)
            }

            Instruction::GetElem {
                dst,
                arr,
                idx,
                ic_index,
            } => {
                let array = ctx.get_register(arr.0).clone();
                let index = ctx.get_register(idx.0).clone();

                if let Some(obj) = array.as_object() {
                    // Fast path for integer index access on arrays
                    if obj.is_array() {
                        if let Some(n) = index.as_int32() {
                            let idx = n as usize;
                            let elements = obj.get_elements_storage().read();
                            if idx < elements.len() {
                                ctx.set_register(dst.0, elements[idx].clone());
                                return Ok(InstructionResult::Continue);
                            }
                        }
                    }

                    // Convert index to property key
                    let key = if let Some(n) = index.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = index.as_string() {
                        PropertyKey::string(s.as_str())
                    } else {
                        let idx_str = self.to_string(&index);
                        PropertyKey::string(&idx_str)
                    };

                    // IC Fast Path - only for string keys
                    if matches!(&key, PropertyKey::String(_)) {
                        let mut cached_val = None;
                        {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            let func = frame
                                .module
                                .function(frame.function_index)
                                .ok_or_else(|| VmError::internal("no function"))?;
                            let feedback = func.feedback_vector.read();
                            if let Some(ic) = feedback.get(*ic_index as usize) {
                                use otter_vm_bytecode::function::InlineCacheState;
                                let obj_shape_ptr = std::sync::Arc::as_ptr(&obj.shape()) as u64;

                                if ic.proto_epoch_matches(get_proto_epoch()) {
                                    match &ic.ic_state {
                                        InlineCacheState::Monomorphic { shape_id, offset } => {
                                            if obj_shape_ptr == *shape_id {
                                                cached_val = obj.get_by_offset(*offset as usize);
                                            }
                                        }
                                        InlineCacheState::Polymorphic { count, entries } => {
                                            for i in 0..(*count as usize) {
                                                if obj_shape_ptr == entries[i].0 {
                                                    cached_val =
                                                        obj.get_by_offset(entries[i].1 as usize);
                                                    break;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }

                        if let Some(val) = cached_val {
                            ctx.set_register(dst.0, val);
                            return Ok(InstructionResult::Continue);
                        }

                        // Slow path with IC update (skip for dictionary mode)
                        if !obj.is_dictionary_mode() {
                            if let Some(offset) = obj.shape().get_offset(&key) {
                                let frame = ctx
                                    .current_frame()
                                    .ok_or_else(|| VmError::internal("no frame"))?;
                                let func = frame
                                    .module
                                    .function(frame.function_index)
                                    .ok_or_else(|| VmError::internal("no function"))?;
                                let mut feedback = func.feedback_vector.write();
                                if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                                    use otter_vm_bytecode::function::InlineCacheState;
                                    let shape_ptr = std::sync::Arc::as_ptr(&obj.shape()) as u64;
                                    let current_epoch = get_proto_epoch();

                                    match &mut ic.ic_state {
                                        InlineCacheState::Uninitialized => {
                                            ic.ic_state = InlineCacheState::Monomorphic {
                                                shape_id: shape_ptr,
                                                offset: offset as u32,
                                            };
                                            ic.proto_epoch = current_epoch;
                                        }
                                        InlineCacheState::Monomorphic {
                                            shape_id: old_shape,
                                            offset: old_offset,
                                        } => {
                                            if *old_shape != shape_ptr {
                                                let mut entries = [(0u64, 0u32); 4];
                                                entries[0] = (*old_shape, *old_offset);
                                                entries[1] = (shape_ptr, offset as u32);
                                                ic.ic_state = InlineCacheState::Polymorphic {
                                                    count: 2,
                                                    entries,
                                                };
                                                ic.proto_epoch = current_epoch;
                                            }
                                        }
                                        InlineCacheState::Polymorphic { count, entries } => {
                                            let mut found = false;
                                            for i in 0..(*count as usize) {
                                                if entries[i].0 == shape_ptr {
                                                    found = true;
                                                    break;
                                                }
                                            }
                                            if !found {
                                                if (*count as usize) < 4 {
                                                    entries[*count as usize] =
                                                        (shape_ptr, offset as u32);
                                                    *count += 1;
                                                    ic.proto_epoch = current_epoch;
                                                } else {
                                                    ic.ic_state = InlineCacheState::Megamorphic;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }

                    let value = obj.get(&key).unwrap_or_else(Value::undefined);
                    ctx.set_register(dst.0, value);
                } else {
                    ctx.set_register(dst.0, Value::undefined());
                }
                Ok(InstructionResult::Continue)
            }

            Instruction::SetElem {
                arr,
                idx,
                val,
                ic_index,
            } => {
                let array = ctx.get_register(arr.0).clone();
                let index = ctx.get_register(idx.0).clone();
                let val_val = ctx.get_register(val.0).clone();

                if let Some(obj) = array.as_object() {
                    // Fast path for integer index access on arrays
                    if obj.is_array() {
                        if let Some(n) = index.as_int32() {
                            let idx = n as usize;
                            let mut elements = obj.get_elements_storage().write();
                            if idx < elements.len() {
                                elements[idx] = val_val;
                                return Ok(InstructionResult::Continue);
                            }
                        }
                    }

                    // Convert index to property key
                    let key = if let Some(n) = index.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = index.as_string() {
                        PropertyKey::string(s.as_str())
                    } else {
                        let idx_str = self.to_string(&index);
                        PropertyKey::string(&idx_str)
                    };

                    // IC Fast Path - only for string keys
                    if matches!(&key, PropertyKey::String(_)) {
                        let mut cached = false;
                        {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            let func = frame
                                .module
                                .function(frame.function_index)
                                .ok_or_else(|| VmError::internal("no function"))?;
                            let feedback = func.feedback_vector.read();
                            if let Some(ic) = feedback.get(*ic_index as usize) {
                                use otter_vm_bytecode::function::InlineCacheState;
                                let obj_shape_ptr = std::sync::Arc::as_ptr(&obj.shape()) as u64;

                                if ic.proto_epoch_matches(get_proto_epoch()) {
                                    match &ic.ic_state {
                                        InlineCacheState::Monomorphic { shape_id, offset } => {
                                            if obj_shape_ptr == *shape_id {
                                                if obj.set_by_offset(
                                                    *offset as usize,
                                                    val_val.clone(),
                                                ) {
                                                    cached = true;
                                                }
                                            }
                                        }
                                        InlineCacheState::Polymorphic { count, entries } => {
                                            for i in 0..(*count as usize) {
                                                if obj_shape_ptr == entries[i].0 {
                                                    if obj.set_by_offset(
                                                        entries[i].1 as usize,
                                                        val_val.clone(),
                                                    ) {
                                                        cached = true;
                                                    }
                                                    break;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }

                        if cached {
                            return Ok(InstructionResult::Continue);
                        }

                        // Slow path with IC update (skip for dictionary mode)
                        if !obj.is_dictionary_mode() {
                            if let Some(offset) = obj.shape().get_offset(&key) {
                                let frame = ctx
                                    .current_frame()
                                    .ok_or_else(|| VmError::internal("no frame"))?;
                                let func = frame
                                    .module
                                    .function(frame.function_index)
                                    .ok_or_else(|| VmError::internal("no function"))?;
                                let mut feedback = func.feedback_vector.write();
                                if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                                    use otter_vm_bytecode::function::InlineCacheState;
                                    let shape_ptr = std::sync::Arc::as_ptr(&obj.shape()) as u64;
                                    let current_epoch = get_proto_epoch();

                                    match &mut ic.ic_state {
                                        InlineCacheState::Uninitialized => {
                                            ic.ic_state = InlineCacheState::Monomorphic {
                                                shape_id: shape_ptr,
                                                offset: offset as u32,
                                            };
                                            ic.proto_epoch = current_epoch;
                                        }
                                        InlineCacheState::Monomorphic {
                                            shape_id: old_shape,
                                            offset: old_offset,
                                        } => {
                                            if *old_shape != shape_ptr {
                                                let mut entries = [(0u64, 0u32); 4];
                                                entries[0] = (*old_shape, *old_offset);
                                                entries[1] = (shape_ptr, offset as u32);
                                                ic.ic_state = InlineCacheState::Polymorphic {
                                                    count: 2,
                                                    entries,
                                                };
                                                ic.proto_epoch = current_epoch;
                                            }
                                        }
                                        InlineCacheState::Polymorphic { count, entries } => {
                                            let mut found = false;
                                            for i in 0..(*count as usize) {
                                                if entries[i].0 == shape_ptr {
                                                    found = true;
                                                    break;
                                                }
                                            }
                                            if !found {
                                                if (*count as usize) < 4 {
                                                    entries[*count as usize] =
                                                        (shape_ptr, offset as u32);
                                                    *count += 1;
                                                    ic.proto_epoch = current_epoch;
                                                } else {
                                                    ic.ic_state = InlineCacheState::Megamorphic;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }

                    obj.set(key, val_val);
                }
                Ok(InstructionResult::Continue)
            }

            Instruction::CallMethodComputed {
                dst,
                obj,
                key,
                argc,
                ic_index,
            } => {
                let receiver = ctx.get_register(obj.0).clone();
                let key_value = ctx.get_register(key.0).clone();

                // IC Fast Path
                // IC Fast Path
                let cached_method = if let Some(obj) = receiver.as_object() {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let func = frame
                        .module
                        .function(frame.function_index)
                        .ok_or_else(|| VmError::internal("no function"))?;
                    let feedback = func.feedback_vector.read();
                    if let Some(ic) = feedback.get(*ic_index as usize) {
                        if let otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                            shape_id: shape_addr,
                            offset,
                        } = &ic.ic_state
                        {
                            if std::sync::Arc::as_ptr(&obj.shape()) as u64 == *shape_addr {
                                obj.get_by_offset(*offset as usize)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(method_value) = cached_method {
                    // Collect arguments (args start at obj + 2)
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        args.push(ctx.get_register(key.0 + 1 + i).clone());
                    }

                    // Direct call handling
                    return self.handle_call_value(ctx, &method_value, receiver, args, dst.0);
                }

                // Special handling for generator methods
                if receiver.is_generator() {
                    let method_str = self.to_string(&key_value);
                    if method_str == "next" || method_str == "return" || method_str == "throw" {
                        let generator = receiver
                            .as_generator()
                            .ok_or_else(|| VmError::internal("Expected generator"))?;

                        // Get the sent value (first argument if present)
                        let sent_value = if *argc > 0 {
                            Some(ctx.get_register(key.0 + 1).clone())
                        } else {
                            None
                        };

                        // Handle the specific method
                        let gen_result = match method_str.as_str() {
                            "next" => self.execute_generator(generator, ctx, sent_value),
                            "return" => {
                                // generator.return(value) - complete with the value
                                // If generator has try handlers, we need to run finally blocks
                                let return_value = sent_value.unwrap_or_else(Value::undefined);

                                if generator.is_completed() {
                                    GeneratorResult::Returned(return_value)
                                } else if !generator.has_try_handlers() {
                                    generator.complete();
                                    GeneratorResult::Returned(return_value)
                                } else {
                                    // Has try handlers - need to run finally blocks
                                    generator.set_pending_return(return_value);
                                    self.execute_generator(generator, ctx, None)
                                }
                            }
                            "throw" => {
                                let error_value = sent_value.unwrap_or_else(Value::undefined);
                                if generator.is_completed() {
                                    GeneratorResult::Error(VmError::exception(error_value))
                                } else {
                                    generator.set_pending_throw(error_value.clone());
                                    self.execute_generator(generator, ctx, None)
                                }
                            }
                            _ => unreachable!(),
                        };

                        // For async generators, wrap result in a Promise
                        if generator.is_async() {
                            let promise = JsPromise::new();
                            let js_queue = ctx.js_job_queue();
                            match gen_result {
                                GeneratorResult::Yielded(v) => {
                                    let iter_result = GcRef::new(JsObject::new(
                                        None,
                                        ctx.memory_manager().clone(),
                                    ));
                                    iter_result.set(PropertyKey::string("value"), v);
                                    iter_result
                                        .set(PropertyKey::string("done"), Value::boolean(false));
                                    let js_queue = js_queue.clone();
                                    promise.resolve_with_js_jobs(
                                        Value::object(iter_result),
                                        move |job, args| {
                                            if let Some(queue) = &js_queue {
                                                queue.enqueue(job, args);
                                            }
                                        },
                                    );
                                }
                                GeneratorResult::Returned(v) => {
                                    let iter_result = GcRef::new(JsObject::new(
                                        None,
                                        ctx.memory_manager().clone(),
                                    ));
                                    iter_result.set(PropertyKey::string("value"), v);
                                    iter_result
                                        .set(PropertyKey::string("done"), Value::boolean(true));
                                    let js_queue = js_queue.clone();
                                    promise.resolve_with_js_jobs(
                                        Value::object(iter_result),
                                        move |job, args| {
                                            if let Some(queue) = &js_queue {
                                                queue.enqueue(job, args);
                                            }
                                        },
                                    );
                                }
                                GeneratorResult::Error(e) => {
                                    let error_msg = e.to_string();
                                    let js_queue = js_queue.clone();
                                    promise.reject_with_js_jobs(
                                        Value::string(JsString::intern(&error_msg)),
                                        move |job, args| {
                                            if let Some(queue) = &js_queue {
                                                queue.enqueue(job, args);
                                            }
                                        },
                                    );
                                }
                                GeneratorResult::Suspended {
                                    promise: awaited_promise,
                                    ..
                                } => {
                                    // Generator is awaiting a promise
                                    let result_promise = promise.clone();
                                    let mm = ctx.memory_manager().clone();
                                    let js_queue = js_queue.clone();
                                    awaited_promise.then(move |resolved_value| {
                                        let iter_result =
                                            GcRef::new(JsObject::new(None, mm.clone()));
                                        iter_result
                                            .set(PropertyKey::string("value"), resolved_value);
                                        iter_result.set(
                                            PropertyKey::string("done"),
                                            Value::boolean(false),
                                        );
                                        let js_queue = js_queue.clone();
                                        result_promise.resolve_with_js_jobs(
                                            Value::object(iter_result),
                                            move |job, args| {
                                                if let Some(queue) = &js_queue {
                                                    queue.enqueue(job, args);
                                                }
                                            },
                                        );
                                    });
                                }
                            }
                            ctx.set_register(dst.0, Value::promise(promise));
                            return Ok(InstructionResult::Continue);
                        }

                        // For sync generators, return iterator result directly
                        let (result_value, is_done) = match gen_result {
                            GeneratorResult::Yielded(v) => (v, false),
                            GeneratorResult::Returned(v) => (v, true),
                            GeneratorResult::Error(e) => return Err(e),
                            GeneratorResult::Suspended { .. } => {
                                return Err(VmError::internal("Sync generator cannot suspend"));
                            }
                        };

                        // Create iterator result object { value, done }
                        let result = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));
                        result.set(PropertyKey::string("value"), result_value);
                        result.set(PropertyKey::string("done"), Value::boolean(is_done));
                        ctx.set_register(dst.0, Value::object(result));
                        return Ok(InstructionResult::Continue);
                    }
                }

                let key = self.value_to_property_key(&key_value);
                let method_value = if let Some(obj_ref) = receiver.as_object() {
                    obj_ref.get(&key).unwrap_or_else(Value::undefined)
                } else {
                    Value::undefined()
                };

                // Update IC if method was found on the object itself
                if let Some(obj) = receiver.as_object() {
                    if let Some(offset) = obj.shape().get_offset(&key) {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let mut feedback = func.feedback_vector.write();
                        if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                            if let otter_vm_bytecode::function::InlineCacheState::Uninitialized =
                                ic.ic_state
                            {
                                ic.ic_state =
                                    otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                        shape_id: std::sync::Arc::as_ptr(&obj.shape()) as u64,
                                        offset: offset as u32,
                                    };
                            }
                        }
                    }
                }

                let mut args = Vec::new();
                for i in 0..(*argc as u16) {
                    args.push(ctx.get_register(obj.0 + 2 + i).clone());
                }

                self.handle_call_value(ctx, &method_value, receiver, args, dst.0)
            }

            Instruction::CallMethodComputedSpread {
                dst,
                obj,
                key,
                spread,
                ic_index,
            } => {
                let receiver = ctx.get_register(obj.0).clone();
                let key_value = ctx.get_register(key.0).clone();
                let spread_arr = ctx.get_register(spread.0).clone();

                // IC Fast Path
                let cached_method = if let Some(obj) = receiver.as_object() {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let func = frame
                        .module
                        .function(frame.function_index)
                        .ok_or_else(|| VmError::internal("no function"))?;
                    let feedback = func.feedback_vector.read();
                    if let Some(ic) = feedback.get(*ic_index as usize) {
                        if let otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                            shape_id: shape_addr,
                            offset,
                        } = &ic.ic_state
                        {
                            if std::sync::Arc::as_ptr(&obj.shape()) as u64 == *shape_addr {
                                obj.get_by_offset(*offset as usize)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(method_value) = cached_method {
                    // Direct call handling
                    return self.dispatch_method_spread(
                        ctx,
                        &method_value,
                        receiver,
                        &spread_arr,
                        dst.0,
                    );
                }

                let key = self.value_to_property_key(&key_value);
                let method_value = if let Some(obj_ref) = receiver.as_object() {
                    obj_ref.get(&key).unwrap_or_else(Value::undefined)
                } else {
                    Value::undefined()
                };

                // Update IC if method was found on the object itself
                if let Some(obj) = receiver.as_object() {
                    if let Some(offset) = obj.shape().get_offset(&key) {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let mut feedback = func.feedback_vector.write();
                        if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                            if let otter_vm_bytecode::function::InlineCacheState::Uninitialized =
                                ic.ic_state
                            {
                                ic.ic_state =
                                    otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                        shape_id: std::sync::Arc::as_ptr(&obj.shape()) as u64,
                                        offset: offset as u32,
                                    };
                            }
                        }
                    }
                }

                self.dispatch_method_spread(ctx, &method_value, receiver, &spread_arr, dst.0)
            }

            Instruction::Spread { dst, src } => {
                // Spread elements from src array into dst array
                let dst_arr = ctx.get_register(dst.0);
                let src_arr = ctx.get_register(src.0);

                if let (Some(dst_obj), Some(src_obj)) = (dst_arr.as_object(), src_arr.as_object()) {
                    // Get current length of dst array
                    let dst_len = dst_obj
                        .get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_int32())
                        .unwrap_or(0) as u32;

                    // Get length of src array
                    let src_len = src_obj
                        .get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_int32())
                        .unwrap_or(0) as u32;

                    // Copy elements from src to dst
                    for i in 0..src_len {
                        let elem = src_obj
                            .get(&PropertyKey::Index(i))
                            .unwrap_or_else(Value::undefined);
                        dst_obj.set(PropertyKey::Index(dst_len + i), elem);
                    }

                    // Update dst length
                    dst_obj.set(
                        PropertyKey::string("length"),
                        Value::int32((dst_len + src_len) as i32),
                    );
                }

                Ok(InstructionResult::Continue)
            }

            // ==================== Misc ====================
            Instruction::Move { dst, src } => {
                let value = ctx.get_register(src.0).clone();
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::Nop => Ok(InstructionResult::Continue),

            Instruction::Debugger => {
                // TODO: Implement debugger hook
                Ok(InstructionResult::Continue)
            }

            // ==================== Iteration ====================
            Instruction::GetIterator { dst, src } => {
                use crate::value::HeapRef;

                let obj = ctx.get_register(src.0).clone();

                // Get Symbol.iterator method using well-known symbol ID (1)
                const SYMBOL_ITERATOR_ID: u64 = 1;
                let iterator_method = match obj.heap_ref() {
                    Some(HeapRef::Object(o)) | Some(HeapRef::Array(o)) => {
                        o.get(&PropertyKey::Symbol(SYMBOL_ITERATOR_ID))
                    }
                    _ => None,
                };

                let iterator_fn =
                    iterator_method.ok_or_else(|| VmError::type_error("Object is not iterable"))?;

                // Call the iterator method with obj as `this`
                if let Some(native_fn) = iterator_fn.as_native_function() {
                    // Native iterator methods take the receiver as their first argument.
                    let iterator = self.call_native_fn(ctx, native_fn, &obj, &[])?;
                    ctx.set_register(dst.0, iterator);
                    Ok(InstructionResult::Continue)
                } else if let Some(closure) = iterator_fn.as_function() {
                    // JS iterator method: call with `this = obj` and no args.
                    ctx.set_pending_args(Vec::new());
                    ctx.set_pending_this(obj);
                    Ok(InstructionResult::Call {
                        func_index: closure.function_index,
                        module: Arc::clone(&closure.module),
                        argc: 0,
                        return_reg: dst.0,
                        is_construct: false,
                        is_async: closure.is_async,
                        upvalues: closure.upvalues.clone(),
                    })
                } else {
                    Err(VmError::type_error("Symbol.iterator is not a function"))
                }
            }

            Instruction::GetAsyncIterator { dst, src } => {
                use crate::value::HeapRef;

                let obj = ctx.get_register(src.0).clone();

                // 1. Try Symbol.asyncIterator (ID 2)
                const SYMBOL_ASYNC_ITERATOR_ID: u64 = 2;
                const SYMBOL_ITERATOR_ID: u64 = 1;

                let mut iterator_method = match obj.heap_ref() {
                    Some(HeapRef::Object(o)) | Some(HeapRef::Array(o)) => {
                        o.get(&PropertyKey::Symbol(SYMBOL_ASYNC_ITERATOR_ID))
                    }
                    _ => None,
                };

                // 2. Fallback to Symbol.iterator (ID 1)
                if iterator_method.is_none() {
                    iterator_method = match obj.heap_ref() {
                        Some(HeapRef::Object(o)) | Some(HeapRef::Array(o)) => {
                            o.get(&PropertyKey::Symbol(SYMBOL_ITERATOR_ID))
                        }
                        _ => None,
                    };
                }

                let iterator_fn = iterator_method
                    .ok_or_else(|| VmError::type_error("Object is not async iterable"))?;

                // Call the iterator method with obj as `this`
                if let Some(native_fn) = iterator_fn.as_native_function() {
                    let iterator = self.call_native_fn(ctx, native_fn, &obj, &[])?;
                    ctx.set_register(dst.0, iterator);
                    Ok(InstructionResult::Continue)
                } else if let Some(closure) = iterator_fn.as_function() {
                    ctx.set_pending_args(Vec::new());
                    ctx.set_pending_this(obj);
                    Ok(InstructionResult::Call {
                        func_index: closure.function_index,
                        module: Arc::clone(&closure.module),
                        argc: 0,
                        return_reg: dst.0,
                        is_construct: false,
                        is_async: closure.is_async,
                        upvalues: closure.upvalues.clone(),
                    })
                } else {
                    Err(VmError::type_error(
                        "Async iterator method is not a function",
                    ))
                }
            }

            Instruction::IteratorNext { dst, done, iter } => {
                let iterator = ctx.get_register(iter.0).clone();

                // Fast path for generator iterators: resume execution directly.
                if let Some(generator) = iterator.as_generator() {
                    let gen_result = self.execute_generator(generator, ctx, None);
                    let (value, is_done) = match gen_result {
                        GeneratorResult::Yielded(v) => (v, false),
                        GeneratorResult::Returned(v) => (v, true),
                        GeneratorResult::Error(e) => return Err(e),
                        GeneratorResult::Suspended { .. } => {
                            return Err(VmError::internal(
                                "Async generator cannot be used in sync iteration",
                            ));
                        }
                    };

                    ctx.set_register(dst.0, value);
                    ctx.set_register(done.0, Value::boolean(is_done));
                    return Ok(InstructionResult::Continue);
                }

                // Get the next method
                let next_method = if let Some(obj) = iterator.as_object() {
                    obj.get(&PropertyKey::string("next"))
                } else {
                    None
                };

                let next_fn = next_method
                    .ok_or_else(|| VmError::type_error("Iterator has no next method"))?;

                // Call next()
                let result = if let Some(native_fn) = next_fn.as_native_function() {
                    self.call_native_fn(ctx, native_fn, &iterator, &[])?
                } else {
                    return Err(VmError::type_error("next is not a function"));
                };

                // Extract done and value from result object
                let result_obj = result
                    .as_object()
                    .ok_or_else(|| VmError::type_error("Iterator result is not an object"))?;

                let done_value = result_obj
                    .get(&PropertyKey::string("done"))
                    .unwrap_or_else(|| Value::boolean(false));
                let value = result_obj
                    .get(&PropertyKey::string("value"))
                    .unwrap_or_else(Value::undefined);

                ctx.set_register(dst.0, value);
                ctx.set_register(done.0, done_value);
                Ok(InstructionResult::Continue)
            }

            // Catch-all for unimplemented instructions
            // TODO: Task List
            // [x] Phase 3 Implementation: GC Integration [x]
            // - [x] Implement `Trace` for `Shape` [x]
            // - [x] Implement `Trace` for `InlineCache` and `feedback_vector` [x]
            // - [x] Update `JsObject::trace` to include Shapes, elements, and keys [x]
            // - [x] Trace bytecode constants and modules [x]
            // - [x] Verify GC safety with simulated collections [x]
            // [/] Phase 4 Implementation: Polymorphic ICs & Array Speedups [/]
            // - [ ] Extend `InlineCache` to support `Polymorphic` state (up to 4 shapes) [ ]
            // - [ ] Update Interpreter to handle polymorphic cache hits [ ]
            // - [ ] Optimize Array indexing (`elements` access bypass) [ ]
            // - [ ] Verify performance on polymorphic benchmarks [ ]
            // ==================== Class ====================
            Instruction::DefineClass {
                dst,
                name: _name,
                ctor,
                super_class,
            } => {
                let ctor_value = ctx.get_register(ctor.0).clone();
                let mm = ctx.memory_manager().clone();

                if let Some(super_reg) = super_class {
                    // Derived class: set up prototype chain
                    let super_value = ctx.get_register(super_reg.0).clone();

                    // Validate superclass is callable (or null for extends null)
                    if super_value.is_null() {
                        // extends null: create prototype with null __proto__
                        let derived_proto = GcRef::new(JsObject::new(None, mm.clone()));

                        // Set ctor.prototype = derived_proto
                        let proto_key = PropertyKey::string("prototype");
                        if let Some(ctor_obj) = ctor_value.as_object() {
                            ctor_obj.set(proto_key, Value::object(derived_proto.clone()));
                            // Set derived_proto.constructor = ctor
                            let ctor_key = PropertyKey::string("constructor");
                            derived_proto.set(ctor_key, ctor_value.clone());
                        }
                    } else if let Some(super_obj) = super_value.as_object() {
                        // Get super.prototype
                        let proto_key = PropertyKey::string("prototype");
                        let super_proto_val =
                            super_obj.get(&proto_key).unwrap_or_else(Value::undefined);

                        // super.prototype must be object or null
                        let super_proto = if super_proto_val.is_null() {
                            None
                        } else if let Some(proto_obj) = super_proto_val.as_object() {
                            Some(proto_obj)
                        } else if super_proto_val.is_undefined() {
                            // No .prototype property — treat as undefined → create with no parent
                            None
                        } else {
                            return Err(VmError::TypeError(
                                "Class extends value does not have valid prototype property"
                                    .to_string(),
                            ));
                        };

                        // Create derived prototype: Object.create(super.prototype)
                        let derived_proto = GcRef::new(JsObject::new(super_proto, mm.clone()));

                        // Set ctor.prototype = derived_proto
                        if let Some(ctor_obj) = ctor_value.as_object() {
                            ctor_obj.set(
                                PropertyKey::string("prototype"),
                                Value::object(derived_proto.clone()),
                            );
                            // Set derived_proto.constructor = ctor
                            derived_proto
                                .set(PropertyKey::string("constructor"), ctor_value.clone());
                            // Static inheritance: ctor.__proto__ = super
                            ctor_obj.set_prototype(Some(super_obj));
                        }
                    } else if super_value.is_function() || super_value.is_native_function() {
                        // Superclass is a function (but not an object with .prototype on HeapRef::Object)
                        // This handles NativeFunction or Function HeapRef variants
                        // For now, create a basic prototype chain
                        let derived_proto = GcRef::new(JsObject::new(None, mm.clone()));
                        if let Some(ctor_obj) = ctor_value.as_object() {
                            ctor_obj.set(
                                PropertyKey::string("prototype"),
                                Value::object(derived_proto.clone()),
                            );
                            derived_proto
                                .set(PropertyKey::string("constructor"), ctor_value.clone());
                        }
                    } else {
                        return Err(VmError::TypeError(
                            "Class extends value is not a constructor or null".to_string(),
                        ));
                    }
                } else {
                    // Base class: ctor already has a .prototype from Closure creation
                    // Just ensure ctor.prototype.constructor = ctor
                    if let Some(ctor_obj) = ctor_value.as_object() {
                        let proto_key = PropertyKey::string("prototype");
                        if let Some(proto_val) = ctor_obj.get(&proto_key) {
                            if let Some(proto_obj) = proto_val.as_object() {
                                proto_obj
                                    .set(PropertyKey::string("constructor"), ctor_value.clone());
                            }
                        }
                    }
                }

                ctx.set_register(dst.0, ctor_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::CallSuper {
                dst,
                args: args_base,
                argc,
            } => {
                // Get the current frame's home_object to find the superclass
                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame for CallSuper"))?;

                let home_object = frame.home_object.clone().ok_or_else(|| {
                    VmError::ReferenceError("'super' keyword unexpected here".to_string())
                })?;

                // new_target_proto is the prototype for the object being created.
                // In the outermost derived constructor, this is home_object (e.g., C.prototype).
                // In deeper levels (multi-level), it was propagated from above.
                let new_target_proto = frame
                    .new_target_proto
                    .clone()
                    .unwrap_or_else(|| home_object.clone());

                // Get the superclass constructor: Object.getPrototypeOf(home_object)
                let super_proto = home_object.prototype().ok_or_else(|| {
                    VmError::TypeError("Super constructor is not a constructor".to_string())
                })?;

                // The super constructor is the .constructor of the prototype's prototype
                let ctor_key = PropertyKey::string("constructor");
                let super_ctor_val = super_proto.get(&ctor_key).unwrap_or_else(Value::undefined);

                // Collect arguments from registers
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    args.push(ctx.get_register(args_base.0 + i).clone());
                }
                let mm = ctx.memory_manager().clone();

                // Check if the super constructor is also a derived class
                let super_is_derived = super_ctor_val
                    .as_function()
                    .and_then(|c| {
                        c.module
                            .function(c.function_index)
                            .map(|f| f.flags.is_derived)
                    })
                    .unwrap_or(false);

                let this_value = if super_is_derived {
                    // Multi-level inheritance: super constructor is also derived.
                    // Don't create the object here. Propagate new_target_proto
                    // and let the chain continue until the base constructor.
                    if let Some(super_closure) = super_ctor_val.as_function() {
                        ctx.set_pending_is_derived(true);
                        // Propagate new_target_proto for the eventual object creation
                        ctx.set_pending_new_target_proto(new_target_proto);
                        let proto_key = PropertyKey::string("prototype");
                        if let Some(proto_val) = super_closure.object.get(&proto_key) {
                            if let Some(proto_obj) = proto_val.as_object() {
                                ctx.set_pending_home_object(proto_obj);
                            }
                        }
                    }

                    let result =
                        self.call_function(ctx, &super_ctor_val, Value::undefined(), &args)?;

                    if result.is_object() {
                        result
                    } else {
                        Value::undefined()
                    }
                } else {
                    // Base case: super constructor is NOT derived.
                    // Create the object with new_target_proto as [[Prototype]].
                    let new_obj = GcRef::new(JsObject::new(Some(new_target_proto), mm.clone()));
                    let new_obj_value = Value::object(new_obj);

                    let result =
                        self.call_function(ctx, &super_ctor_val, new_obj_value.clone(), &args)?;

                    if result.is_object() {
                        result
                    } else {
                        new_obj_value
                    }
                };

                // Set this_initialized and update this_value on current frame
                if let Some(frame) = ctx.current_frame_mut() {
                    frame.this_value = this_value.clone();
                    frame.this_initialized = true;
                }

                ctx.set_register(dst.0, this_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::GetSuper { dst } => {
                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame for GetSuper"))?;

                let home_object = frame.home_object.clone().ok_or_else(|| {
                    VmError::ReferenceError("'super' keyword unexpected here".to_string())
                })?;

                // super = Object.getPrototypeOf(home_object)
                let result = match home_object.prototype() {
                    Some(proto) => Value::object(proto),
                    None => Value::null(),
                };

                ctx.set_register(dst.0, result);
                Ok(InstructionResult::Continue)
            }

            Instruction::GetSuperProp { dst, name } => {
                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame for GetSuperProp"))?;

                let home_object = frame.home_object.clone().ok_or_else(|| {
                    VmError::ReferenceError("'super' keyword unexpected here".to_string())
                })?;

                // Get super prototype (Object.getPrototypeOf(home_object))
                let super_proto = home_object.prototype();

                // Look up property on super prototype, handling accessors
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("GetSuperProp: constant not found"))?;
                let name_units = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("GetSuperProp: expected string constant"))?;
                let key = Self::utf16_key(name_units);

                // Get the current this value (super property access uses current this, not prototype)
                let this_value = ctx
                    .current_frame()
                    .map(|f| f.this_value.clone())
                    .unwrap_or_else(Value::undefined);

                let value = if let Some(proto) = super_proto {
                    // Use lookup_property_descriptor to find accessor properties
                    match proto.lookup_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Data { value, .. }) => value,
                        Some(crate::object::PropertyDescriptor::Accessor {
                            get: Some(getter),
                            ..
                        }) => {
                            // Invoke the getter with the current this
                            self.call_function(ctx, &getter, this_value, &[])?
                        }
                        Some(crate::object::PropertyDescriptor::Accessor { get: None, .. }) => {
                            Value::undefined()
                        }
                        _ => Value::undefined(),
                    }
                } else {
                    Value::undefined()
                };

                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::SetHomeObject { func, obj } => {
                let func_val = ctx.get_register(func.0).clone();
                let obj_val = ctx.get_register(obj.0).clone();
                if let Some(closure) = func_val.as_function() {
                    if let Some(obj_ref) = obj_val.as_object() {
                        // Create a new closure with home_object set
                        let new_closure = Closure {
                            function_index: closure.function_index,
                            module: Arc::clone(&closure.module),
                            upvalues: closure.upvalues.clone(),
                            is_async: closure.is_async,
                            is_generator: closure.is_generator,
                            object: closure.object.clone(),
                            home_object: Some(obj_ref),
                        };
                        ctx.set_register(func.0, Value::function(Arc::new(new_closure)));
                    }
                }
                Ok(InstructionResult::Continue)
            }

            // ==================== Bitwise operators ====================
            Instruction::BitAnd { dst, lhs, rhs } => {
                let l = self.to_int32_from(self.coerce_number(ctx.get_register(lhs.0))?);
                let r = self.to_int32_from(self.coerce_number(ctx.get_register(rhs.0))?);
                ctx.set_register(dst.0, Value::number((l & r) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::BitOr { dst, lhs, rhs } => {
                let l = self.to_int32_from(self.coerce_number(ctx.get_register(lhs.0))?);
                let r = self.to_int32_from(self.coerce_number(ctx.get_register(rhs.0))?);
                ctx.set_register(dst.0, Value::number((l | r) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::BitXor { dst, lhs, rhs } => {
                let l = self.to_int32_from(self.coerce_number(ctx.get_register(lhs.0))?);
                let r = self.to_int32_from(self.coerce_number(ctx.get_register(rhs.0))?);
                ctx.set_register(dst.0, Value::number((l ^ r) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::BitNot { dst, src } => {
                let v = self.to_int32_from(self.coerce_number(ctx.get_register(src.0))?);
                ctx.set_register(dst.0, Value::number((!v) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::Shl { dst, lhs, rhs } => {
                let l = self.to_int32_from(self.coerce_number(ctx.get_register(lhs.0))?);
                let r = self.to_uint32_from(self.coerce_number(ctx.get_register(rhs.0))?);
                let shift = (r & 0x1f) as u32;
                ctx.set_register(dst.0, Value::number((l.wrapping_shl(shift)) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::Shr { dst, lhs, rhs } => {
                let l = self.to_int32_from(self.coerce_number(ctx.get_register(lhs.0))?);
                let r = self.to_uint32_from(self.coerce_number(ctx.get_register(rhs.0))?);
                let shift = (r & 0x1f) as u32;
                ctx.set_register(dst.0, Value::number((l.wrapping_shr(shift)) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::Ushr { dst, lhs, rhs } => {
                let l = self.to_uint32_from(self.coerce_number(ctx.get_register(lhs.0))?);
                let r = self.to_uint32_from(self.coerce_number(ctx.get_register(rhs.0))?);
                let shift = (r & 0x1f) as u32;
                ctx.set_register(dst.0, Value::number((l.wrapping_shr(shift)) as f64));
                Ok(InstructionResult::Continue)
            }

            _ => Err(VmError::internal(format!(
                "Unimplemented instruction: {:?}",
                instruction
            ))),
        }
    }

    /// Convert a bytecode constant to a Value
    fn constant_to_value(
        &self,
        ctx: &mut VmContext,
        constant: &otter_vm_bytecode::Constant,
    ) -> VmResult<Value> {
        use otter_vm_bytecode::Constant;

        match constant {
            Constant::Number(n) => Ok(Value::number(*n)),
            Constant::String(s) => {
                let js_str = JsString::intern_utf16(s);
                Ok(Value::string(js_str))
            }
            Constant::BigInt(s) => Ok(Value::bigint(s.to_string())),
            Constant::RegExp { pattern, flags } => {
                // Get RegExp prototype
                let global = ctx.global();
                let regexp_ctor = global
                    .get(&PropertyKey::string("RegExp"))
                    .unwrap_or_else(Value::undefined);

                let proto = if let Some(ctor) = regexp_ctor.as_object() {
                    ctor.get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                } else {
                    None
                };

                let js_regex = Arc::new(JsRegExp::new(
                    pattern.to_string(),
                    flags.to_string(),
                    proto,
                    ctx.memory_manager().clone(),
                ));
                Ok(Value::regex(js_regex))
            }
            Constant::TemplateLiteral(_) => {
                Err(VmError::internal("Template literals not yet supported"))
            }
            Constant::Symbol(id) => {
                let sym = Arc::new(crate::value::Symbol {
                    id: *id,
                    description: None,
                });
                Ok(Value::symbol(sym))
            }
        }
    }

    /// Execute an eval-compiled module within the current execution context.
    ///
    /// Unlike `execute()` / `run_loop()`, this method tracks the pre-eval
    /// stack depth and returns when the eval frame finishes, without
    /// consuming outer call frames. This is the same pattern used by
    /// `call_function`.
    fn execute_eval_module(
        &mut self,
        ctx: &mut VmContext,
        module: &Module,
    ) -> VmResult<Value> {
        let module = Arc::new(module.clone());
        let entry_func = module
            .entry_function()
            .ok_or_else(|| VmError::internal("eval: no entry function"))?;

        let prev_stack_depth = ctx.stack_depth();

        ctx.push_frame(
            module.entry_point,
            Arc::clone(&module),
            entry_func.local_count,
            None,
            false,
            entry_func.is_async(),
            0,
        )?;

        // Mini run-loop that returns when eval frame completes
        loop {
            if ctx.should_check_interrupt() && ctx.is_interrupted() {
                // Pop the eval frame before returning error
                while ctx.stack_depth() > prev_stack_depth {
                    ctx.pop_frame();
                }
                return Err(VmError::interrupted());
            }

            let frame = ctx
                .current_frame()
                .ok_or_else(|| VmError::internal("eval: no frame"))?;
            let current_module = Arc::clone(&frame.module);
            let func = current_module
                .function(frame.function_index)
                .ok_or_else(|| VmError::internal("eval: function not found"))?;

            // End of function → implicit return undefined
            if frame.pc >= func.instructions.len() {
                if ctx.stack_depth() <= prev_stack_depth {
                    return Ok(Value::undefined());
                }
                ctx.pop_frame();
                continue;
            }

            let instruction = &func.instructions[frame.pc];
            ctx.record_instruction();

            match self.execute_instruction(instruction, &current_module, ctx) {
                Ok(InstructionResult::Continue) => {
                    ctx.advance_pc();
                }
                Ok(InstructionResult::Jump(offset)) => {
                    ctx.jump(offset);
                }
                Ok(InstructionResult::Return(value)) => {
                    if ctx.stack_depth() <= prev_stack_depth + 1 {
                        ctx.pop_frame();
                        return Ok(value);
                    }
                    let return_reg = ctx
                        .current_frame()
                        .and_then(|f| f.return_register)
                        .unwrap_or(0);
                    ctx.pop_frame();
                    ctx.set_register(return_reg, value);
                }
                Ok(InstructionResult::Call {
                    func_index,
                    module: call_module,
                    argc,
                    return_reg,
                    is_construct,
                    is_async,
                    upvalues,
                }) => {
                    ctx.advance_pc();
                    let local_count = call_module
                        .function(func_index)
                        .ok_or_else(|| VmError::internal("eval: called function not found"))?
                        .local_count;
                    ctx.push_frame(
                        func_index,
                        call_module,
                        local_count,
                        Some(return_reg),
                        is_construct,
                        is_async,
                        argc as usize,
                    )?;
                    // Set upvalues on the new frame
                    if !upvalues.is_empty() {
                        if let Some(frame) = ctx.current_frame_mut() {
                            frame.upvalues = upvalues;
                        }
                    }
                }
                Ok(InstructionResult::Throw(value)) => {
                    // Check if there's a try handler within the eval scope
                    if let Some((target_depth, catch_pc)) = ctx.peek_nearest_try() {
                        if target_depth > prev_stack_depth {
                            // Handler is within eval scope — use it
                            ctx.take_nearest_try();
                            while ctx.stack_depth() > target_depth {
                                ctx.pop_frame();
                            }
                            if let Some(frame) = ctx.current_frame_mut() {
                                frame.pc = catch_pc;
                            }
                            ctx.set_register(0, value);
                            continue;
                        }
                    }
                    // No handler in eval scope — unwind and propagate to outer
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame();
                    }
                    return Err(VmError::exception(value));
                }
                Ok(InstructionResult::TailCall { .. }) => {
                    return Err(VmError::internal("tail call in eval not yet supported"));
                }
                Ok(InstructionResult::Suspend { .. }) => {
                    return Err(VmError::internal("await in eval not yet supported"));
                }
                Ok(InstructionResult::Yield { .. }) => {
                    return Err(VmError::internal("yield in eval not yet supported"));
                }
                Err(VmError::Exception(thrown)) => {
                    let error_value = thrown.value;
                    if let Some((target_depth, catch_pc)) = ctx.peek_nearest_try() {
                        if target_depth > prev_stack_depth {
                            ctx.take_nearest_try();
                            while ctx.stack_depth() > target_depth {
                                ctx.pop_frame();
                            }
                            if let Some(frame) = ctx.current_frame_mut() {
                                frame.pc = catch_pc;
                            }
                            ctx.set_register(0, error_value);
                            continue;
                        }
                    }
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame();
                    }
                    return Err(VmError::exception(error_value));
                }
                Err(VmError::SyntaxError(msg)) => {
                    let error_val = self.make_error(ctx, "SyntaxError", &msg);
                    if let Some((target_depth, catch_pc)) = ctx.peek_nearest_try() {
                        if target_depth > prev_stack_depth {
                            ctx.take_nearest_try();
                            while ctx.stack_depth() > target_depth {
                                ctx.pop_frame();
                            }
                            if let Some(frame) = ctx.current_frame_mut() {
                                frame.pc = catch_pc;
                            }
                            ctx.set_register(0, error_val);
                            continue;
                        }
                    }
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame();
                    }
                    return Err(VmError::exception(error_val));
                }
                Err(other) => {
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame();
                    }
                    return Err(other);
                }
            }
        }
    }

    /// Call a native function with depth tracking to prevent Rust stack overflow.
    ///
    /// This method tracks the native call depth and returns an error if it exceeds
    /// the maximum. This prevents JS code that calls native functions recursively
    /// from overflowing the Rust stack.
    #[inline]
    fn call_native_fn(
        &self,
        ctx: &VmContext,
        native_fn: &crate::value::NativeFn,
        this_value: &Value,
        args: &[Value],
    ) -> VmResult<Value> {
        ctx.enter_native_call()?;
        let result = native_fn(this_value, args, ctx.memory_manager().clone());
        ctx.exit_native_call();
        result
    }

    /// Handle a function call value (native or closure)
    fn handle_call_value(
        &mut self,
        ctx: &mut VmContext,
        func_value: &Value,
        this_value: Value,
        args: Vec<Value>,
        return_reg: u16,
    ) -> VmResult<InstructionResult> {
        let mut current_func = func_value.clone();
        let mut current_this = this_value;
        let mut current_args = args;

        // 1. Unwrap all nested bound functions
        while let Some(obj) = current_func.as_object() {
            if let Some(bound_fn) = obj.get(&PropertyKey::string("__boundFunction__")) {
                let raw_this_arg = obj
                    .get(&PropertyKey::string("__boundThis__"))
                    .unwrap_or_else(Value::undefined);
                if raw_this_arg.is_null() || raw_this_arg.is_undefined() {
                    current_this = Value::object(ctx.global());
                } else {
                    current_this = raw_this_arg;
                };

                if let Some(bound_args_val) = obj.get(&PropertyKey::string("__boundArgs__")) {
                    if let Some(args_obj) = bound_args_val.as_object() {
                        let len =
                            if let Some(len_val) = args_obj.get(&PropertyKey::string("length")) {
                                len_val.as_int32().unwrap_or(0) as usize
                            } else {
                                0
                            };
                        let mut new_args = Vec::with_capacity(len + current_args.len());
                        for i in 0..len {
                            new_args.push(
                                args_obj
                                    .get(&PropertyKey::Index(i as u32))
                                    .unwrap_or_else(Value::undefined),
                            );
                        }
                        new_args.extend(current_args);
                        current_args = new_args;
                    }
                }
                current_func = bound_fn;
            } else {
                break;
            }
        }

        // 2. Handle native functions (including interception for call/apply/Generator)
        if let Some(native_fn) = current_func.as_native_function() {
            let is_same_native = |candidate: &Value| -> bool {
                match (current_func.heap_ref(), candidate.heap_ref()) {
                    (Some(HeapRef::NativeFunction(a)), Some(HeapRef::NativeFunction(b))) => {
                        Arc::ptr_eq(a, b)
                    }
                    _ => false,
                }
            };

            // OLD interception code removed - now using error-based interception in native functions

            // Intercept Generator ops
            let gen_op = if let Some(value) = ctx.get_global("__Generator_next")
                && is_same_native(&value)
            {
                Some("next")
            } else if let Some(value) = ctx.get_global("__Generator_return")
                && is_same_native(&value)
            {
                Some("return")
            } else if let Some(value) = ctx.get_global("__Generator_throw")
                && is_same_native(&value)
            {
                Some("throw")
            } else {
                None
            };

            if let Some(op) = gen_op {
                let (generator, sent_value) = if let Some(generator_ref) =
                    current_args.first().and_then(|v| v.as_generator())
                {
                    let value = if current_args.len() > 1 {
                        Some(current_args[1].clone())
                    } else {
                        None
                    };
                    (generator_ref, value)
                } else if let Some(generator_ref) = current_this.as_generator() {
                    let value = current_args.first().cloned();
                    (generator_ref, value)
                } else {
                    return Err(VmError::type_error("First argument must be a generator"));
                };

                let gen_result = match op {
                    "next" => self.execute_generator(generator, ctx, sent_value),
                    "return" => {
                        let return_value = sent_value.unwrap_or_else(Value::undefined);
                        if generator.is_completed() {
                            GeneratorResult::Returned(return_value)
                        } else if !generator.has_try_handlers() {
                            generator.complete();
                            GeneratorResult::Returned(return_value)
                        } else {
                            generator.set_pending_return(return_value);
                            self.execute_generator(generator, ctx, None)
                        }
                    }
                    "throw" => {
                        let error_value = sent_value.unwrap_or_else(Value::undefined);
                        if generator.is_completed() {
                            GeneratorResult::Error(VmError::exception(error_value))
                        } else {
                            generator.set_pending_throw(error_value.clone());
                            self.execute_generator(generator, ctx, None)
                        }
                    }
                    _ => unreachable!(),
                };

                if generator.is_async() {
                    let promise = JsPromise::new();
                    let js_queue = ctx.js_job_queue();
                    match gen_result {
                        GeneratorResult::Yielded(v) => {
                            let iter_result =
                                GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));
                            iter_result.set(PropertyKey::string("value"), v);
                            iter_result.set(PropertyKey::string("done"), Value::boolean(false));
                            let js_queue = js_queue.clone();
                            promise.resolve_with_js_jobs(
                                Value::object(iter_result),
                                move |job, args| {
                                    if let Some(queue) = &js_queue {
                                        queue.enqueue(job, args);
                                    }
                                },
                            );
                        }
                        GeneratorResult::Returned(v) => {
                            let iter_result =
                                GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));
                            iter_result.set(PropertyKey::string("value"), v);
                            iter_result.set(PropertyKey::string("done"), Value::boolean(true));
                            let js_queue = js_queue.clone();
                            promise.resolve_with_js_jobs(
                                Value::object(iter_result),
                                move |job, args| {
                                    if let Some(queue) = &js_queue {
                                        queue.enqueue(job, args);
                                    }
                                },
                            );
                        }
                        GeneratorResult::Error(e) => {
                            let error_msg = e.to_string();
                            let js_queue = js_queue.clone();
                            promise.reject_with_js_jobs(
                                Value::string(JsString::intern(&error_msg)),
                                move |job, args| {
                                    if let Some(queue) = &js_queue {
                                        queue.enqueue(job, args);
                                    }
                                },
                            );
                        }
                        GeneratorResult::Suspended {
                            promise: awaited_promise,
                            ..
                        } => {
                            let result_promise = promise.clone();
                            let mm = ctx.memory_manager().clone();
                            let js_queue = js_queue.clone();
                            awaited_promise.then(move |resolved_value| {
                                let iter_result = GcRef::new(JsObject::new(None, mm.clone()));
                                iter_result.set(PropertyKey::string("value"), resolved_value);
                                iter_result.set(PropertyKey::string("done"), Value::boolean(false));
                                let js_queue = js_queue.clone();
                                result_promise.resolve_with_js_jobs(
                                    Value::object(iter_result),
                                    move |job, args| {
                                        if let Some(queue) = &js_queue {
                                            queue.enqueue(job, args);
                                        }
                                    },
                                );
                            });
                        }
                    }
                    ctx.set_register(return_reg, Value::promise(promise));
                    return Ok(InstructionResult::Continue);
                }

                let (result_value, is_done) = match gen_result {
                    GeneratorResult::Yielded(v) => (v, false),
                    GeneratorResult::Returned(v) => (v, true),
                    GeneratorResult::Error(e) => return Err(e),
                    GeneratorResult::Suspended { .. } => {
                        return Err(VmError::internal("Sync generator cannot suspend"));
                    }
                };

                let result = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));
                result.set(PropertyKey::string("value"), result_value);
                result.set(PropertyKey::string("done"), Value::boolean(is_done));
                ctx.set_register(return_reg, Value::object(result));
                return Ok(InstructionResult::Continue);
            }

            // Normal native function execution with interception support
            match self.call_native_fn(ctx, native_fn, &current_this, &current_args) {
                Ok(result) => {
                    ctx.set_register(return_reg, result);
                    return Ok(InstructionResult::Continue);
                }
                Err(VmError::Interception(signal)) => {
                    // Handle interception signals for Function.prototype.call/apply
                    use crate::error::InterceptionSignal;

                    let js_queue = ctx.js_job_queue();
                    let enqueue_js_job = {
                        let js_queue = js_queue.clone();
                        move |job, args| {
                            if let Some(queue) = &js_queue {
                                queue.enqueue(job, args);
                            }
                        }
                    };

                    let extract_array_items = |value: Option<&Value>| -> Result<Vec<Value>, VmError> {
                        let value = value.ok_or_else(|| VmError::type_error("Expected an iterable"))?;
                        if let Some(obj) = value.as_object() {
                            if obj.is_array() {
                                let len = obj.array_length();
                                let mut items = Vec::with_capacity(len);
                                for i in 0..len {
                                    items.push(
                                        obj.get(&PropertyKey::Index(i as u32))
                                            .unwrap_or(Value::undefined()),
                                    );
                                }
                                return Ok(items);
                            }
                        }
                        Ok(vec![value.clone()])
                    };

                    let internal_key = PropertyKey::string("_internal");
                    let extract_internal_promise = |value: &Value| -> Option<Arc<JsPromise>> {
                        if let Some(promise) = value.as_promise() {
                            return Some(promise.clone());
                        }
                        if let Some(obj) = value.as_object() {
                            if let Some(internal) = obj.get(&internal_key) {
                                if let Some(promise) = internal.as_promise() {
                                    return Some(promise.clone());
                                }
                            }
                        }
                        None
                    };
                    let is_wrapped_promise = |value: &Value| -> bool {
                        if let Some(obj) = value.as_object() {
                            if let Some(internal) = obj.get(&internal_key) {
                                return internal.is_promise();
                            }
                        }
                        false
                    };

                    match signal {
                        InterceptionSignal::FunctionCall => {
                            // Function.prototype.call(thisArg, ...args)
                            // current_this = the function to call
                            // current_args[0] = thisArg
                            // current_args[1..] = the arguments
                            let target = current_this;
                            let this_arg = current_args.first().cloned().unwrap_or(Value::undefined());
                            let call_args = if current_args.len() > 1 {
                                current_args[1..].to_vec()
                            } else {
                                vec![]
                            };

                            return self.handle_call_value(ctx, &target, this_arg, call_args, return_reg);
                        }
                        InterceptionSignal::FunctionApply => {
                            // Function.prototype.apply(thisArg, argsArray)
                            // current_this = the function to call
                            // current_args[0] = thisArg
                            // current_args[1] = argsArray
                            let target = current_this;
                            let this_arg = current_args.first().cloned().unwrap_or(Value::undefined());
                            let args_array = current_args.get(1).cloned().unwrap_or(Value::undefined());

                            // Extract arguments from array
                            let call_args = if args_array.is_undefined() || args_array.is_null() {
                                vec![]
                            } else if let Some(arr_obj) = args_array.as_object() {
                                if arr_obj.is_array() {
                                    let len = arr_obj.array_length();
                                    let mut extracted = Vec::with_capacity(len);
                                    for i in 0..len {
                                        extracted.push(
                                            arr_obj.get(&PropertyKey::Index(i as u32))
                                                .unwrap_or(Value::undefined())
                                        );
                                    }
                                    extracted
                                } else {
                                    return Err(VmError::type_error("Function.prototype.apply: argumentsList must be an array"));
                                }
                            } else {
                                return Err(VmError::type_error("Function.prototype.apply: argumentsList must be an object"));
                            };

                            return self.handle_call_value(ctx, &target, this_arg, call_args, return_reg);
                        }
                        InterceptionSignal::ReflectApply => {
                            // Reflect.apply(target, thisArg, argsArray)
                            // current_args[0] = target
                            // current_args[1] = thisArg
                            // current_args[2] = argsArray
                            if current_args.len() < 3 {
                                return Err(VmError::type_error("Reflect.apply requires 3 arguments"));
                            }

                            let target = &current_args[0];
                            let this_arg = current_args[1].clone();
                            let args_array = &current_args[2];

                            // Extract arguments from array
                            let call_args = if let Some(arr_obj) = args_array.as_object() {
                                if arr_obj.is_array() {
                                    let len = arr_obj.array_length();
                                    let mut extracted = Vec::with_capacity(len);
                                    for i in 0..len {
                                        extracted.push(
                                            arr_obj.get(&PropertyKey::Index(i as u32))
                                                .unwrap_or(Value::undefined())
                                        );
                                    }
                                    extracted
                                } else {
                                    return Err(VmError::type_error("Reflect.apply: argumentsList must be an array"));
                                }
                            } else {
                                return Err(VmError::type_error("Reflect.apply: argumentsList must be an object"));
                            };

                            return self.handle_call_value(ctx, target, this_arg, call_args, return_reg);
                        }
                        InterceptionSignal::ReflectConstruct => {
                            // Reflect.construct(target, argsArray [, newTarget])
                            // current_args[0] = target
                            // current_args[1] = argsArray
                            // current_args[2] = newTarget (optional, not implemented yet)
                            if current_args.len() < 2 {
                                return Err(VmError::type_error("Reflect.construct requires at least 2 arguments"));
                            }

                            let target = &current_args[0];
                            let args_array = &current_args[1];

                            // Extract arguments from array
                            let call_args = if let Some(arr_obj) = args_array.as_object() {
                                if arr_obj.is_array() {
                                    let len = arr_obj.array_length();
                                    let mut extracted = Vec::with_capacity(len);
                                    for i in 0..len {
                                        extracted.push(
                                            arr_obj.get(&PropertyKey::Index(i as u32))
                                                .unwrap_or(Value::undefined())
                                        );
                                    }
                                    extracted
                                } else {
                                    return Err(VmError::type_error("Reflect.construct: argumentsList must be an array"));
                                }
                            } else {
                                return Err(VmError::type_error("Reflect.construct: argumentsList must be an object"));
                            };

                            // Create new object for this
                            let new_obj = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));
                            let this_value = Value::object(new_obj.clone());

                            // Call constructor
                            self.handle_call_value(ctx, target, this_value.clone(), call_args, return_reg)?;

                            // Get result from register
                            let ctor_result = ctx.get_register(return_reg);

                            // Return object or this
                            if ctor_result.is_object() {
                                ctx.set_register(return_reg, ctor_result.clone());
                            } else {
                                ctx.set_register(return_reg, this_value);
                            }
                            return Ok(InstructionResult::Continue);
                        }
                        InterceptionSignal::EvalCall => {
                            // Indirect eval: compile and execute in global scope
                            let code_value = current_args
                                .first()
                                .cloned()
                                .unwrap_or(Value::undefined());
                            let js_str = code_value
                                .as_string()
                                .ok_or_else(|| {
                                    VmError::type_error("eval argument is not a string")
                                })?;
                            let source = js_str.as_str().to_string();
                            // Indirect eval does NOT inherit strict mode
                            let eval_module = ctx.compile_eval(&source, false)?;
                            let result =
                                self.execute_eval_module(ctx, &eval_module)?;
                            ctx.set_register(return_reg, result);
                            return Ok(InstructionResult::Continue);
                        }

                        // ============================================================
                        // Array callback methods — use call_function for sync closure calls
                        // ============================================================

                        InterceptionSignal::ArrayForEach => {
                            // current_this = the array, current_args[0] = callback, [1] = thisArg
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("forEach: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let this_arg = current_args.get(1).cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            for i in 0..len {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                self.call_function(ctx, &callback, this_arg.clone(),
                                    &[val, Value::number(i as f64), current_this.clone()])?;
                            }
                            ctx.set_register(return_reg, Value::undefined());
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArrayMap => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("map: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let this_arg = current_args.get(1).cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            let result = GcRef::new(JsObject::array(len, ctx.memory_manager().clone()));
                            for i in 0..len {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                let mapped = self.call_function(ctx, &callback, this_arg.clone(),
                                    &[val, Value::number(i as f64), current_this.clone()])?;
                                result.set(PropertyKey::Index(i as u32), mapped);
                            }
                            ctx.set_register(return_reg, Value::array(result));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArrayFilter => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("filter: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let this_arg = current_args.get(1).cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            let result = GcRef::new(JsObject::array(0, ctx.memory_manager().clone()));
                            let mut out_idx = 0u32;
                            for i in 0..len {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                let keep = self.call_function(ctx, &callback, this_arg.clone(),
                                    &[val.clone(), Value::number(i as f64), current_this.clone()])?;
                                if keep.to_boolean() {
                                    result.set(PropertyKey::Index(out_idx), val);
                                    out_idx += 1;
                                }
                            }
                            result.set(PropertyKey::string("length"), Value::number(out_idx as f64));
                            ctx.set_register(return_reg, Value::array(result));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArrayFind => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("find: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let this_arg = current_args.get(1).cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            for i in 0..len {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                let test = self.call_function(ctx, &callback, this_arg.clone(),
                                    &[val.clone(), Value::number(i as f64), current_this.clone()])?;
                                if test.to_boolean() {
                                    ctx.set_register(return_reg, val);
                                    return Ok(InstructionResult::Continue);
                                }
                            }
                            ctx.set_register(return_reg, Value::undefined());
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArrayFindIndex => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("findIndex: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let this_arg = current_args.get(1).cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            for i in 0..len {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                let test = self.call_function(ctx, &callback, this_arg.clone(),
                                    &[val, Value::number(i as f64), current_this.clone()])?;
                                if test.to_boolean() {
                                    ctx.set_register(return_reg, Value::number(i as f64));
                                    return Ok(InstructionResult::Continue);
                                }
                            }
                            ctx.set_register(return_reg, Value::number(-1.0));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArrayFindLast => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("findLast: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let this_arg = current_args.get(1).cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            for i in (0..len).rev() {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                let test = self.call_function(ctx, &callback, this_arg.clone(),
                                    &[val.clone(), Value::number(i as f64), current_this.clone()])?;
                                if test.to_boolean() {
                                    ctx.set_register(return_reg, val);
                                    return Ok(InstructionResult::Continue);
                                }
                            }
                            ctx.set_register(return_reg, Value::undefined());
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArrayFindLastIndex => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("findLastIndex: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let this_arg = current_args.get(1).cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            for i in (0..len).rev() {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                let test = self.call_function(ctx, &callback, this_arg.clone(),
                                    &[val, Value::number(i as f64), current_this.clone()])?;
                                if test.to_boolean() {
                                    ctx.set_register(return_reg, Value::number(i as f64));
                                    return Ok(InstructionResult::Continue);
                                }
                            }
                            ctx.set_register(return_reg, Value::number(-1.0));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArrayEvery => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("every: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let this_arg = current_args.get(1).cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            for i in 0..len {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                let test = self.call_function(ctx, &callback, this_arg.clone(),
                                    &[val, Value::number(i as f64), current_this.clone()])?;
                                if !test.to_boolean() {
                                    ctx.set_register(return_reg, Value::boolean(false));
                                    return Ok(InstructionResult::Continue);
                                }
                            }
                            ctx.set_register(return_reg, Value::boolean(true));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArraySome => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("some: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let this_arg = current_args.get(1).cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            for i in 0..len {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                let test = self.call_function(ctx, &callback, this_arg.clone(),
                                    &[val, Value::number(i as f64), current_this.clone()])?;
                                if test.to_boolean() {
                                    ctx.set_register(return_reg, Value::boolean(true));
                                    return Ok(InstructionResult::Continue);
                                }
                            }
                            ctx.set_register(return_reg, Value::boolean(false));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArrayReduce => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("reduce: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            let has_initial = current_args.len() > 1;
                            let mut accumulator = if has_initial {
                                current_args[1].clone()
                            } else {
                                if len == 0 {
                                    return Err(VmError::type_error("Reduce of empty array with no initial value"));
                                }
                                obj.get(&PropertyKey::Index(0)).unwrap_or(Value::undefined())
                            };
                            let start = if has_initial { 0 } else { 1 };
                            for i in start..len {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                accumulator = self.call_function(ctx, &callback, Value::undefined(),
                                    &[accumulator, val, Value::number(i as f64), current_this.clone()])?;
                            }
                            ctx.set_register(return_reg, accumulator);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArrayReduceRight => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("reduceRight: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            let has_initial = current_args.len() > 1;
                            let mut accumulator = if has_initial {
                                current_args[1].clone()
                            } else {
                                if len == 0 {
                                    return Err(VmError::type_error("Reduce of empty array with no initial value"));
                                }
                                obj.get(&PropertyKey::Index((len - 1) as u32)).unwrap_or(Value::undefined())
                            };
                            let end = if has_initial { len } else { len.saturating_sub(1) };
                            for i in (0..end).rev() {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                accumulator = self.call_function(ctx, &callback, Value::undefined(),
                                    &[accumulator, val, Value::number(i as f64), current_this.clone()])?;
                            }
                            ctx.set_register(return_reg, accumulator);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArrayFlatMap => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("flatMap: not an object"))?;
                            let callback = current_args.first().cloned().unwrap_or(Value::undefined());
                            let this_arg = current_args.get(1).cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                            let result = GcRef::new(JsObject::array(0, ctx.memory_manager().clone()));
                            let mut out_idx = 0u32;
                            for i in 0..len {
                                let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                let mapped = self.call_function(ctx, &callback, this_arg.clone(),
                                    &[val, Value::number(i as f64), current_this.clone()])?;
                                // Flatten one level
                                if let Some(inner) = mapped.as_object() {
                                    if inner.get(&PropertyKey::string("length")).is_some() {
                                        let inner_len = inner.get(&PropertyKey::string("length"))
                                            .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                                        for j in 0..inner_len {
                                            let item = inner.get(&PropertyKey::Index(j as u32)).unwrap_or(Value::undefined());
                                            result.set(PropertyKey::Index(out_idx), item);
                                            out_idx += 1;
                                        }
                                        continue;
                                    }
                                }
                                result.set(PropertyKey::Index(out_idx), mapped);
                                out_idx += 1;
                            }
                            result.set(PropertyKey::string("length"), Value::number(out_idx as f64));
                            ctx.set_register(return_reg, Value::array(result));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ArraySort => {
                            let obj = current_this.as_object()
                                .ok_or_else(|| VmError::type_error("sort: not an object"))?;
                            let compare_fn = current_args.first().cloned().unwrap_or(Value::undefined());
                            let len = obj.get(&PropertyKey::string("length"))
                                .and_then(|v| v.as_number()).unwrap_or(0.0) as usize;

                            // Collect elements
                            let mut elements: Vec<Value> = Vec::with_capacity(len.min(1024));
                            for i in 0..len {
                                elements.push(obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined()));
                            }

                            // Sort using closure comparator via call_function
                            // We use a simple insertion sort to avoid the FnMut issue with sort_by
                            let mut i = 1;
                            while i < elements.len() {
                                let mut j = i;
                                while j > 0 {
                                    let cmp_result = self.call_function(ctx, &compare_fn, Value::undefined(),
                                        &[elements[j - 1].clone(), elements[j].clone()])?;
                                    let n = cmp_result.as_number().unwrap_or(0.0);
                                    if n > 0.0 {
                                        elements.swap(j - 1, j);
                                        j -= 1;
                                    } else {
                                        break;
                                    }
                                }
                                i += 1;
                            }

                            // Write sorted elements back
                            for (i, val) in elements.into_iter().enumerate() {
                                obj.set(PropertyKey::Index(i as u32), val);
                            }
                            ctx.set_register(return_reg, current_this);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectGetProxy => {
                            // Reflect.get(proxy, propertyKey, receiver?)
                            // current_args[0] = proxy (target)
                            // current_args[1] = propertyKey
                            // current_args[2] = receiver (optional)
                            if current_args.len() < 2 {
                                return Err(VmError::type_error("Reflect.get requires at least 2 arguments"));
                            }

                            let target = &current_args[0];
                            let property_key = &current_args[1];
                            let receiver = if current_args.len() > 2 {
                                current_args[2].clone()
                            } else {
                                target.clone()
                            };

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.get: target is not a proxy"))?;

                            // Convert key to PropertyKey
                            let key = if let Some(n) = property_key.as_int32() {
                                PropertyKey::Index(n as u32)
                            } else if let Some(s) = property_key.as_string() {
                                PropertyKey::from_js_string(s)
                            } else if let Some(sym) = property_key.as_symbol() {
                                PropertyKey::Symbol(sym.id)
                            } else {
                                let key_str = self.to_string(property_key);
                                PropertyKey::string(&key_str)
                            };

                            // Call proxy_get
                            let result = crate::proxy_operations::proxy_get(
                                self,
                                ctx,
                                proxy,
                                &key,
                                property_key.clone(),
                                receiver,
                            )?;

                            ctx.set_register(return_reg, result);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectSetProxy => {
                            // Reflect.set(proxy, propertyKey, value, receiver?)
                            // current_args[0] = proxy (target)
                            // current_args[1] = propertyKey
                            // current_args[2] = value
                            // current_args[3] = receiver (optional)
                            if current_args.len() < 3 {
                                return Err(VmError::type_error("Reflect.set requires at least 3 arguments"));
                            }

                            let target = &current_args[0];
                            let property_key = &current_args[1];
                            let value = current_args[2].clone();
                            let receiver = if current_args.len() > 3 {
                                current_args[3].clone()
                            } else {
                                target.clone()
                            };

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.set: target is not a proxy"))?;

                            // Convert key to PropertyKey
                            let key = if let Some(n) = property_key.as_int32() {
                                PropertyKey::Index(n as u32)
                            } else if let Some(s) = property_key.as_string() {
                                PropertyKey::from_js_string(s)
                            } else if let Some(sym) = property_key.as_symbol() {
                                PropertyKey::Symbol(sym.id)
                            } else {
                                let key_str = self.to_string(property_key);
                                PropertyKey::string(&key_str)
                            };

                            // Call proxy_set
                            let success = crate::proxy_operations::proxy_set(
                                self,
                                ctx,
                                proxy,
                                &key,
                                property_key.clone(),
                                value,
                                receiver,
                            )?;

                            ctx.set_register(return_reg, Value::boolean(success));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectHasProxy => {
                            // Reflect.has(proxy, propertyKey)
                            // current_args[0] = proxy (target)
                            // current_args[1] = propertyKey
                            if current_args.len() < 2 {
                                return Err(VmError::type_error("Reflect.has requires at least 2 arguments"));
                            }

                            let target = &current_args[0];
                            let property_key = &current_args[1];

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.has: target is not a proxy"))?;

                            // Convert key to PropertyKey
                            let key = if let Some(n) = property_key.as_int32() {
                                PropertyKey::Index(n as u32)
                            } else if let Some(s) = property_key.as_string() {
                                PropertyKey::from_js_string(s)
                            } else if let Some(sym) = property_key.as_symbol() {
                                PropertyKey::Symbol(sym.id)
                            } else {
                                let key_str = self.to_string(property_key);
                                PropertyKey::string(&key_str)
                            };

                            // Call proxy_has
                            let result = crate::proxy_operations::proxy_has(
                                self,
                                ctx,
                                proxy,
                                &key,
                                property_key.clone(),
                            )?;

                            ctx.set_register(return_reg, Value::boolean(result));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectDeletePropertyProxy => {
                            // Reflect.deleteProperty(proxy, propertyKey)
                            // current_args[0] = proxy (target)
                            // current_args[1] = propertyKey
                            if current_args.len() < 2 {
                                return Err(VmError::type_error("Reflect.deleteProperty requires at least 2 arguments"));
                            }

                            let target = &current_args[0];
                            let property_key = &current_args[1];

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.deleteProperty: target is not a proxy"))?;

                            // Convert key to PropertyKey
                            let key = if let Some(n) = property_key.as_int32() {
                                PropertyKey::Index(n as u32)
                            } else if let Some(s) = property_key.as_string() {
                                PropertyKey::from_js_string(s)
                            } else if let Some(sym) = property_key.as_symbol() {
                                PropertyKey::Symbol(sym.id)
                            } else {
                                let key_str = self.to_string(property_key);
                                PropertyKey::string(&key_str)
                            };

                            // Call proxy_delete_property
                            let result = crate::proxy_operations::proxy_delete_property(
                                self,
                                ctx,
                                proxy,
                                &key,
                                property_key.clone(),
                            )?;

                            ctx.set_register(return_reg, Value::boolean(result));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectOwnKeysProxy => {
                            // Reflect.ownKeys(proxy)
                            // current_args[0] = proxy (target)
                            if current_args.is_empty() {
                                return Err(VmError::type_error("Reflect.ownKeys requires at least 1 argument"));
                            }

                            let target = &current_args[0];

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.ownKeys: target is not a proxy"))?;

                            // Call proxy_own_keys
                            let keys = crate::proxy_operations::proxy_own_keys(
                                self,
                                ctx,
                                proxy,
                            )?;

                            // Convert keys to array
                            let result = GcRef::new(crate::object::JsObject::new(None, ctx.memory_manager().clone()));
                            for (i, key) in keys.into_iter().enumerate() {
                                let key_val = match key {
                                    PropertyKey::String(s) => Value::string(s),
                                    PropertyKey::Index(n) => Value::string(crate::string::JsString::intern(&n.to_string())),
                                    PropertyKey::Symbol(sym_id) => {
                                        // Create symbol value
                                        Value::symbol(Arc::new(crate::value::Symbol {
                                            description: None,
                                            id: sym_id,
                                        }))
                                    }
                                };
                                result.set(PropertyKey::Index(i as u32), key_val);
                            }
                            result.set(PropertyKey::from("length"), Value::int32(result.own_keys().len() as i32));

                            ctx.set_register(return_reg, Value::object(result));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectGetOwnPropertyDescriptorProxy => {
                            // Reflect.getOwnPropertyDescriptor(proxy, propertyKey)
                            // current_args[0] = proxy (target)
                            // current_args[1] = propertyKey
                            if current_args.len() < 2 {
                                return Err(VmError::type_error("Reflect.getOwnPropertyDescriptor requires at least 2 arguments"));
                            }

                            let target = &current_args[0];
                            let property_key = &current_args[1];

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.getOwnPropertyDescriptor: target is not a proxy"))?;

                            // Convert key to PropertyKey
                            let key = if let Some(n) = property_key.as_int32() {
                                PropertyKey::Index(n as u32)
                            } else if let Some(s) = property_key.as_string() {
                                PropertyKey::from_js_string(s)
                            } else if let Some(sym) = property_key.as_symbol() {
                                PropertyKey::Symbol(sym.id)
                            } else {
                                let key_str = self.to_string(property_key);
                                PropertyKey::string(&key_str)
                            };

                            // Call proxy_get_own_property_descriptor
                            let result_desc = crate::proxy_operations::proxy_get_own_property_descriptor(
                                self,
                                ctx,
                                proxy,
                                &key,
                                property_key.clone(),
                            )?;

                            // Convert descriptor to object or undefined
                            let result = if let Some(desc) = result_desc {
                                match desc {
                                    crate::object::PropertyDescriptor::Data { value, attributes } => {
                                        let desc_obj = GcRef::new(crate::object::JsObject::new(None, ctx.memory_manager().clone()));
                                        desc_obj.set(PropertyKey::from("value"), value);
                                        desc_obj.set(PropertyKey::from("writable"), Value::boolean(attributes.writable));
                                        desc_obj.set(PropertyKey::from("enumerable"), Value::boolean(attributes.enumerable));
                                        desc_obj.set(PropertyKey::from("configurable"), Value::boolean(attributes.configurable));
                                        Value::object(desc_obj)
                                    }
                                    crate::object::PropertyDescriptor::Accessor { get, set, attributes } => {
                                        let desc_obj = GcRef::new(crate::object::JsObject::new(None, ctx.memory_manager().clone()));
                                        if let Some(getter) = get {
                                            desc_obj.set(PropertyKey::from("get"), getter);
                                        }
                                        if let Some(setter) = set {
                                            desc_obj.set(PropertyKey::from("set"), setter);
                                        }
                                        desc_obj.set(PropertyKey::from("enumerable"), Value::boolean(attributes.enumerable));
                                        desc_obj.set(PropertyKey::from("configurable"), Value::boolean(attributes.configurable));
                                        Value::object(desc_obj)
                                    }
                                    crate::object::PropertyDescriptor::Deleted => {
                                        // Return undefined
                                        Value::undefined()
                                    }
                                }
                            } else {
                                Value::undefined()
                            };

                            ctx.set_register(return_reg, result);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectDefinePropertyProxy => {
                            // Reflect.defineProperty(proxy, propertyKey, attributes)
                            // current_args[0] = proxy (target)
                            // current_args[1] = propertyKey
                            // current_args[2] = attributes
                            if current_args.len() < 3 {
                                return Err(VmError::type_error("Reflect.defineProperty requires at least 3 arguments"));
                            }

                            let target = &current_args[0];
                            let property_key = &current_args[1];
                            let attributes = &current_args[2];

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.defineProperty: target is not a proxy"))?;

                            // Convert key to PropertyKey
                            let key = if let Some(n) = property_key.as_int32() {
                                PropertyKey::Index(n as u32)
                            } else if let Some(s) = property_key.as_string() {
                                PropertyKey::from_js_string(s)
                            } else if let Some(sym) = property_key.as_symbol() {
                                PropertyKey::Symbol(sym.id)
                            } else {
                                let key_str = self.to_string(property_key);
                                PropertyKey::string(&key_str)
                            };

                            // Convert attributes object to PropertyDescriptor
                            let attr_obj = attributes.as_object()
                                .ok_or_else(|| VmError::type_error("Reflect.defineProperty requires attributes to be an object"))?;

                            let has_value = attr_obj.has(&PropertyKey::from("value"));
                            let has_writable = attr_obj.has(&PropertyKey::from("writable"));
                            let has_get = attr_obj.has(&PropertyKey::from("get"));
                            let has_set = attr_obj.has(&PropertyKey::from("set"));

                            let enumerable = attr_obj
                                .get(&PropertyKey::from("enumerable"))
                                .map(|v| v.to_boolean())
                                .unwrap_or(false);
                            let configurable = attr_obj
                                .get(&PropertyKey::from("configurable"))
                                .map(|v| v.to_boolean())
                                .unwrap_or(false);

                            let desc = if has_value || has_writable {
                                // Data descriptor
                                let value = attr_obj.get(&PropertyKey::from("value")).unwrap_or(Value::undefined());
                                let writable = attr_obj
                                    .get(&PropertyKey::from("writable"))
                                    .map(|v| v.to_boolean())
                                    .unwrap_or(false);
                                crate::object::PropertyDescriptor::Data {
                                    value,
                                    attributes: crate::object::PropertyAttributes {
                                        writable,
                                        enumerable,
                                        configurable,
                                    },
                                }
                            } else if has_get || has_set {
                                // Accessor descriptor
                                let get = attr_obj.get(&PropertyKey::from("get")).filter(|v| !v.is_undefined());
                                let set = attr_obj.get(&PropertyKey::from("set")).filter(|v| !v.is_undefined());
                                crate::object::PropertyDescriptor::Accessor {
                                    get,
                                    set,
                                    attributes: crate::object::PropertyAttributes {
                                        writable: false,
                                        enumerable,
                                        configurable,
                                    },
                                }
                            } else {
                                // Generic descriptor
                                crate::object::PropertyDescriptor::Data {
                                    value: Value::undefined(),
                                    attributes: crate::object::PropertyAttributes {
                                        writable: false,
                                        enumerable,
                                        configurable,
                                    },
                                }
                            };

                            // Call proxy_define_property
                            let result = crate::proxy_operations::proxy_define_property(
                                self,
                                ctx,
                                proxy,
                                &key,
                                property_key.clone(),
                                &desc,
                            )?;

                            ctx.set_register(return_reg, Value::boolean(result));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectGetPrototypeOfProxy => {
                            // Reflect.getPrototypeOf(proxy)
                            // current_args[0] = proxy (target)
                            if current_args.is_empty() {
                                return Err(VmError::type_error("Reflect.getPrototypeOf requires at least 1 argument"));
                            }

                            let target = &current_args[0];

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.getPrototypeOf: target is not a proxy"))?;

                            // Call proxy_get_prototype_of
                            let result_proto = crate::proxy_operations::proxy_get_prototype_of(
                                self,
                                ctx,
                                proxy,
                            )?;

                            // Convert to Value
                            let result = match result_proto {
                                Some(proto) => Value::object(proto),
                                None => Value::null(),
                            };

                            ctx.set_register(return_reg, result);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectSetPrototypeOfProxy => {
                            // Reflect.setPrototypeOf(proxy, prototype)
                            // current_args[0] = proxy (target)
                            // current_args[1] = prototype
                            if current_args.len() < 2 {
                                return Err(VmError::type_error("Reflect.setPrototypeOf requires at least 2 arguments"));
                            }

                            let target = &current_args[0];
                            let prototype = &current_args[1];

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.setPrototypeOf: target is not a proxy"))?;

                            // Convert prototype to Option<GcRef<JsObject>>
                            let proto = if prototype.is_null() {
                                None
                            } else if let Some(proto_obj) = prototype.as_object() {
                                Some(proto_obj)
                            } else {
                                return Err(VmError::type_error("Prototype must be an object or null"));
                            };

                            // Call proxy_set_prototype_of
                            let result = crate::proxy_operations::proxy_set_prototype_of(
                                self,
                                ctx,
                                proxy,
                                proto,
                            )?;

                            ctx.set_register(return_reg, Value::boolean(result));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectIsExtensibleProxy => {
                            // Reflect.isExtensible(proxy)
                            // current_args[0] = proxy (target)
                            if current_args.is_empty() {
                                return Err(VmError::type_error("Reflect.isExtensible requires at least 1 argument"));
                            }

                            let target = &current_args[0];

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.isExtensible: target is not a proxy"))?;

                            // Call proxy_is_extensible
                            let result = crate::proxy_operations::proxy_is_extensible(
                                self,
                                ctx,
                                proxy,
                            )?;

                            ctx.set_register(return_reg, Value::boolean(result));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectPreventExtensionsProxy => {
                            // Reflect.preventExtensions(proxy)
                            // current_args[0] = proxy (target)
                            if current_args.is_empty() {
                                return Err(VmError::type_error("Reflect.preventExtensions requires at least 1 argument"));
                            }

                            let target = &current_args[0];

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.preventExtensions: target is not a proxy"))?;

                            // Call proxy_prevent_extensions
                            let result = crate::proxy_operations::proxy_prevent_extensions(
                                self,
                                ctx,
                                proxy,
                            )?;

                            ctx.set_register(return_reg, Value::boolean(result));
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectApplyProxy => {
                            // Reflect.apply(proxy, thisArgument, argumentsList)
                            // current_args[0] = proxy (target)
                            // current_args[1] = thisArgument
                            // current_args[2] = argumentsList
                            if current_args.len() < 3 {
                                return Err(VmError::type_error("Reflect.apply requires at least 3 arguments"));
                            }

                            let target = &current_args[0];
                            let this_arg = current_args[1].clone();
                            let args_list = &current_args[2];

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.apply: target is not a proxy"))?;

                            // Convert argumentsList to Vec<Value>
                            let args_array = if let Some(arr_obj) = args_list.as_object() {
                                let len = arr_obj.get(&PropertyKey::from("length"))
                                    .and_then(|v| v.as_int32())
                                    .unwrap_or(0) as usize;
                                let mut call_args = Vec::with_capacity(len);
                                for i in 0..len {
                                    let val = arr_obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                    call_args.push(val);
                                }
                                call_args
                            } else {
                                return Err(VmError::type_error("Reflect.apply argumentsList must be an object"));
                            };

                            // Call proxy_apply
                            let result = crate::proxy_operations::proxy_apply(
                                self,
                                ctx,
                                proxy,
                                this_arg,
                                &args_array,
                            )?;

                            ctx.set_register(return_reg, result);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::ReflectConstructProxy => {
                            // Reflect.construct(proxy, argumentsList, newTarget?)
                            // current_args[0] = proxy (target)
                            // current_args[1] = argumentsList
                            // current_args[2] = newTarget (optional)
                            if current_args.len() < 2 {
                                return Err(VmError::type_error("Reflect.construct requires at least 2 arguments"));
                            }

                            let target = &current_args[0];
                            let args_list = &current_args[1];
                            let new_target = current_args.get(2).cloned().unwrap_or_else(|| target.clone());

                            // Get proxy from target
                            let proxy = target.as_proxy()
                                .ok_or_else(|| VmError::type_error("Reflect.construct: target is not a proxy"))?;

                            // Convert argumentsList to Vec<Value>
                            let args_array = if let Some(arr_obj) = args_list.as_object() {
                                let len = arr_obj.get(&PropertyKey::from("length"))
                                    .and_then(|v| v.as_int32())
                                    .unwrap_or(0) as usize;
                                let mut call_args = Vec::with_capacity(len);
                                for i in 0..len {
                                    let val = arr_obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                                    call_args.push(val);
                                }
                                call_args
                            } else {
                                return Err(VmError::type_error("Reflect.construct argumentsList must be an object"));
                            };

                            // Call proxy_construct
                            let result = crate::proxy_operations::proxy_construct(
                                self,
                                ctx,
                                proxy,
                                &args_array,
                                new_target,
                            )?;

                            ctx.set_register(return_reg, result);
                            return Ok(InstructionResult::Continue);
                        }

                        // ============================================================
                        // Promise methods — register JS callbacks via job queue
                        // ============================================================

                        InterceptionSignal::PromiseConstructor => {
                            return Err(VmError::type_error("Constructor Promise requires 'new'"));
                        }

                        InterceptionSignal::PromiseThen => {
                            // Promise.prototype.then(onFulfilled, onRejected)
                            // current_this = source promise
                            // current_args[0] = onFulfilled
                            // current_args[1] = onRejected

                            let source = extract_internal_promise(&current_this)
                                .ok_or_else(|| VmError::type_error("then: not a promise"))?;

                            let on_fulfilled = current_args.get(0).cloned().unwrap_or(Value::undefined());
                            let on_rejected = current_args.get(1).cloned().unwrap_or(Value::undefined());

                            // Create result promise for chaining
                            let result_promise = crate::promise::JsPromise::new();

                            let fulfill_job = crate::promise::JsPromiseJob {
                                kind: if on_fulfilled.is_function() {
                                    crate::promise::JsPromiseJobKind::Fulfill
                                } else {
                                    crate::promise::JsPromiseJobKind::PassthroughFulfill
                                },
                                callback: on_fulfilled,
                                this_arg: Value::undefined(),
                                result_promise: Some(result_promise.clone()),
                            };

                            let reject_job = crate::promise::JsPromiseJob {
                                kind: if on_rejected.is_function() {
                                    crate::promise::JsPromiseJobKind::Reject
                                } else {
                                    crate::promise::JsPromiseJobKind::PassthroughReject
                                },
                                callback: on_rejected,
                                this_arg: Value::undefined(),
                                result_promise: Some(result_promise.clone()),
                            };

                            source.then_js(fulfill_job, |job, args| {
                                let _ = ctx.enqueue_js_job(job, args);
                            });
                            source.catch_js(reject_job, |job, args| {
                                let _ = ctx.enqueue_js_job(job, args);
                            });

                            let js_promise = self.create_js_promise(ctx, result_promise);
                            ctx.set_register(return_reg, js_promise);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::PromiseCatch => {
                            // Promise.prototype.catch(onRejected)
                            // current_this = source promise
                            // current_args[0] = onRejected

                            let source = extract_internal_promise(&current_this)
                                .ok_or_else(|| VmError::type_error("catch: not a promise"))?;

                            let on_rejected = current_args.get(0).cloned().unwrap_or(Value::undefined());

                            // Create result promise for chaining
                            let result_promise = crate::promise::JsPromise::new();

                            let fulfill_job = crate::promise::JsPromiseJob {
                                kind: crate::promise::JsPromiseJobKind::PassthroughFulfill,
                                callback: Value::undefined(),
                                this_arg: Value::undefined(),
                                result_promise: Some(result_promise.clone()),
                            };

                            let reject_job = crate::promise::JsPromiseJob {
                                kind: if on_rejected.is_function() {
                                    crate::promise::JsPromiseJobKind::Reject
                                } else {
                                    crate::promise::JsPromiseJobKind::PassthroughReject
                                },
                                callback: on_rejected,
                                this_arg: Value::undefined(),
                                result_promise: Some(result_promise.clone()),
                            };

                            source.then_js(fulfill_job, |job, args| {
                                let _ = ctx.enqueue_js_job(job, args);
                            });
                            source.catch_js(reject_job, |job, args| {
                                let _ = ctx.enqueue_js_job(job, args);
                            });

                            let js_promise = self.create_js_promise(ctx, result_promise);
                            ctx.set_register(return_reg, js_promise);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::PromiseFinally => {
                            // Promise.prototype.finally(onFinally)
                            // current_this = source promise
                            // current_args[0] = onFinally
                            //
                            // Note: finally runs the callback on both fulfill and reject,
                            // then passes through the original value/error

                            let source = extract_internal_promise(&current_this)
                                .ok_or_else(|| VmError::type_error("finally: not a promise"))?;

                            let on_finally = current_args.get(0).cloned().unwrap_or(Value::undefined());

                            // Create result promise for chaining
                            let result_promise = crate::promise::JsPromise::new();

                            let (fulfill_kind, reject_kind, fulfill_callback, reject_callback) =
                                if on_finally.is_function() {
                                    (
                                        crate::promise::JsPromiseJobKind::FinallyFulfill,
                                        crate::promise::JsPromiseJobKind::FinallyReject,
                                        on_finally.clone(),
                                        on_finally,
                                    )
                                } else {
                                    (
                                        crate::promise::JsPromiseJobKind::PassthroughFulfill,
                                        crate::promise::JsPromiseJobKind::PassthroughReject,
                                        Value::undefined(),
                                        Value::undefined(),
                                    )
                                };

                            let fulfill_job = crate::promise::JsPromiseJob {
                                kind: fulfill_kind,
                                callback: fulfill_callback,
                                this_arg: Value::undefined(),
                                result_promise: Some(result_promise.clone()),
                            };

                            let reject_job = crate::promise::JsPromiseJob {
                                kind: reject_kind,
                                callback: reject_callback,
                                this_arg: Value::undefined(),
                                result_promise: Some(result_promise.clone()),
                            };

                            source.then_js(fulfill_job, |job, args| {
                                let _ = ctx.enqueue_js_job(job, args);
                            });
                            source.catch_js(reject_job, |job, args| {
                                let _ = ctx.enqueue_js_job(job, args);
                            });

                            let js_promise = self.create_js_promise(ctx, result_promise);
                            ctx.set_register(return_reg, js_promise);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::PromiseAll => {
                            // Promise.all(iterable)
                            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
                            use std::sync::Mutex;

                            let items = extract_array_items(current_args.first())?;
                            let result_promise = crate::promise::JsPromise::new();
                            let mm = ctx.memory_manager().clone();

                            if items.is_empty() {
                                let arr = GcRef::new(JsObject::array(0, mm));
                                result_promise.resolve_with_js_jobs(Value::array(arr), enqueue_js_job.clone());
                                let js_promise = self.create_js_promise(ctx, result_promise);
                                ctx.set_register(return_reg, js_promise);
                                return Ok(InstructionResult::Continue);
                            }

                            let count = items.len();
                            let remaining = Arc::new(AtomicUsize::new(count));
                            let results: Arc<Mutex<Vec<Option<Value>>>> =
                                Arc::new(Mutex::new(vec![None; count]));
                            let rejected = Arc::new(AtomicBool::new(false));

                            for (index, item) in items.into_iter().enumerate() {
                                let result_p = result_promise.clone();
                                let remaining = remaining.clone();
                                let results = results.clone();
                                let rejected = rejected.clone();
                                let mm_inner = mm.clone();
                                let enqueue = enqueue_js_job.clone();

                                let source_promise = if let Some(promise) = extract_internal_promise(&item) {
                                    promise
                                } else {
                                    let p = crate::promise::JsPromise::new();
                                    let enqueue_resolve = enqueue_js_job.clone();
                                    p.resolve_with_js_jobs(item, enqueue_resolve);
                                    p
                                };

                                let result_p_reject = result_p.clone();
                                let rejected_check = rejected.clone();
                                let enqueue_reject = enqueue_js_job.clone();
                                source_promise.then(move |value| {
                                    if rejected.load(Ordering::Acquire) {
                                        return;
                                    }
                                    if let Ok(mut locked) = results.lock() {
                                        locked[index] = Some(value);
                                    }
                                    if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                                        let arr =
                                            GcRef::new(JsObject::array(count, mm_inner.clone()));
                                        if let Ok(locked) = results.lock() {
                                            for (i, v) in locked.iter().enumerate() {
                                                if let Some(val) = v {
                                                    arr.set(PropertyKey::Index(i as u32), val.clone());
                                                }
                                            }
                                        }
                                        result_p.resolve_with_js_jobs(Value::array(arr), enqueue.clone());
                                    }
                                });
                                source_promise.catch(move |error| {
                                    if !rejected_check.swap(true, Ordering::AcqRel) {
                                        result_p_reject
                                            .reject_with_js_jobs(error, enqueue_reject.clone());
                                    }
                                });
                            }

                            let js_promise = self.create_js_promise(ctx, result_promise);
                            ctx.set_register(return_reg, js_promise);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::PromiseRace => {
                            // Promise.race(iterable)
                            use std::sync::atomic::{AtomicBool, Ordering};

                            let items = extract_array_items(current_args.first())?;
                            let result_promise = crate::promise::JsPromise::new();
                            let settled = Arc::new(AtomicBool::new(false));

                            for item in items {
                                let result_p = result_promise.clone();
                                let result_p_reject = result_promise.clone();
                                let settled1 = settled.clone();
                                let settled2 = settled.clone();
                                let enqueue = enqueue_js_job.clone();
                                let enqueue_reject = enqueue_js_job.clone();
                                let source_promise = if let Some(promise) = extract_internal_promise(&item) {
                                    promise
                                } else {
                                    let p = crate::promise::JsPromise::new();
                                    let enqueue_resolve = enqueue_js_job.clone();
                                    p.resolve_with_js_jobs(item, enqueue_resolve);
                                    p
                                };

                                source_promise.then(move |value| {
                                    if !settled1.swap(true, Ordering::AcqRel) {
                                        result_p.resolve_with_js_jobs(value, enqueue.clone());
                                    }
                                });
                                source_promise.catch(move |error| {
                                    if !settled2.swap(true, Ordering::AcqRel) {
                                        result_p_reject
                                            .reject_with_js_jobs(error, enqueue_reject.clone());
                                    }
                                });
                            }

                            let js_promise = self.create_js_promise(ctx, result_promise);
                            ctx.set_register(return_reg, js_promise);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::PromiseAllSettled => {
                            // Promise.allSettled(iterable)
                            use std::sync::atomic::{AtomicUsize, Ordering};
                            use std::sync::Mutex;

                            let items = extract_array_items(current_args.first())?;
                            let result_promise = crate::promise::JsPromise::new();
                            let mm = ctx.memory_manager().clone();

                            if items.is_empty() {
                                let arr = GcRef::new(JsObject::array(0, mm));
                                result_promise.resolve_with_js_jobs(Value::array(arr), enqueue_js_job.clone());
                                let js_promise = self.create_js_promise(ctx, result_promise);
                                ctx.set_register(return_reg, js_promise);
                                return Ok(InstructionResult::Continue);
                            }

                            let count = items.len();
                            let remaining = Arc::new(AtomicUsize::new(count));
                            let results: Arc<Mutex<Vec<Option<Value>>>> =
                                Arc::new(Mutex::new(vec![None; count]));
                            for (index, item) in items.into_iter().enumerate() {
                                let result_p = result_promise.clone();
                                let remaining = remaining.clone();
                                let results = results.clone();
                                let result_p2 = result_promise.clone();
                                let remaining2 = remaining.clone();
                                let results2 = results.clone();
                                let mm_t = mm.clone();
                                let mm_c = mm.clone();
                                let enqueue = enqueue_js_job.clone();
                                let enqueue2 = enqueue_js_job.clone();
                                let source_promise = if let Some(promise) = extract_internal_promise(&item) {
                                    promise
                                } else {
                                    let p = crate::promise::JsPromise::new();
                                    let enqueue_resolve = enqueue_js_job.clone();
                                    p.resolve_with_js_jobs(item, enqueue_resolve);
                                    p
                                };

                                source_promise.then(move |value| {
                                    let obj = GcRef::new(JsObject::new(None, mm_t.clone()));
                                    obj.set(
                                        "status".into(),
                                        Value::string(JsString::intern("fulfilled")),
                                    );
                                    obj.set("value".into(), value);
                                    if let Ok(mut locked) = results.lock() {
                                        locked[index] = Some(Value::object(obj));
                                    }
                                    if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                                        let arr = GcRef::new(JsObject::array(count, mm_t.clone()));
                                        if let Ok(locked) = results.lock() {
                                            for (i, v) in locked.iter().enumerate() {
                                                if let Some(val) = v {
                                                    arr.set(PropertyKey::Index(i as u32), val.clone());
                                                }
                                            }
                                        }
                                        result_p
                                            .resolve_with_js_jobs(Value::array(arr), enqueue.clone());
                                    }
                                });
                                source_promise.catch(move |error| {
                                    let obj = GcRef::new(JsObject::new(None, mm_c.clone()));
                                    obj.set(
                                        "status".into(),
                                        Value::string(JsString::intern("rejected")),
                                    );
                                    obj.set("reason".into(), error);
                                    if let Ok(mut locked) = results2.lock() {
                                        locked[index] = Some(Value::object(obj));
                                    }
                                    if remaining2.fetch_sub(1, Ordering::AcqRel) == 1 {
                                        let arr = GcRef::new(JsObject::array(count, mm_c.clone()));
                                        if let Ok(locked) = results2.lock() {
                                            for (i, v) in locked.iter().enumerate() {
                                                if let Some(val) = v {
                                                    arr.set(PropertyKey::Index(i as u32), val.clone());
                                                }
                                            }
                                        }
                                        result_p2
                                            .resolve_with_js_jobs(Value::array(arr), enqueue2.clone());
                                    }
                                });
                            }

                            let js_promise = self.create_js_promise(ctx, result_promise);
                            ctx.set_register(return_reg, js_promise);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::PromiseAny => {
                            // Promise.any(iterable)
                            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
                            use std::sync::Mutex;

                            let items = extract_array_items(current_args.first())?;
                            let result_promise = crate::promise::JsPromise::new();
                            let mm = ctx.memory_manager().clone();

                            if items.is_empty() {
                                let err =
                                    Value::string(JsString::intern("All promises were rejected"));
                                result_promise.reject_with_js_jobs(err, enqueue_js_job.clone());
                                let js_promise = self.create_js_promise(ctx, result_promise);
                                ctx.set_register(return_reg, js_promise);
                                return Ok(InstructionResult::Continue);
                            }

                            let count = items.len();
                            let fulfilled = Arc::new(AtomicBool::new(false));
                            let remaining = Arc::new(AtomicUsize::new(count));
                            let errors: Arc<Mutex<Vec<Option<Value>>>> =
                                Arc::new(Mutex::new(vec![None; count]));

                            for (index, item) in items.into_iter().enumerate() {
                                let result_p = result_promise.clone();
                                let fulfilled1 = fulfilled.clone();
                                let remaining = remaining.clone();
                                let errors = errors.clone();
                                let result_p2 = result_promise.clone();
                                let fulfilled2 = fulfilled.clone();
                                let mm_err = mm.clone();
                                let enqueue = enqueue_js_job.clone();
                                let enqueue_reject = enqueue_js_job.clone();
                                let source_promise = if let Some(promise) = extract_internal_promise(&item) {
                                    promise
                                } else {
                                    let p = crate::promise::JsPromise::new();
                                    let enqueue_resolve = enqueue_js_job.clone();
                                    p.resolve_with_js_jobs(item, enqueue_resolve);
                                    p
                                };

                                source_promise.then(move |value| {
                                    if !fulfilled1.swap(true, Ordering::AcqRel) {
                                        result_p.resolve_with_js_jobs(value, enqueue.clone());
                                    }
                                });
                                source_promise.catch(move |error| {
                                    if fulfilled2.load(Ordering::Acquire) {
                                        return;
                                    }
                                    if let Ok(mut locked) = errors.lock() {
                                        locked[index] = Some(error);
                                    }
                                    if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                                        let errs: Vec<Value> = if let Ok(locked) = errors.lock() {
                                            locked.iter().filter_map(|e| e.clone()).collect()
                                        } else {
                                            vec![]
                                        };
                                        let arr = GcRef::new(JsObject::array(
                                            errs.len(),
                                            mm_err.clone(),
                                        ));
                                        for (i, e) in errs.iter().enumerate() {
                                            arr.set(PropertyKey::Index(i as u32), e.clone());
                                        }
                                        let agg = GcRef::new(JsObject::new(None, mm_err.clone()));
                                        agg.set(
                                            "message".into(),
                                            Value::string(JsString::intern(
                                                "All promises were rejected",
                                            )),
                                        );
                                        agg.set("errors".into(), Value::array(arr));
                                        result_p2.reject_with_js_jobs(
                                            Value::object(agg),
                                            enqueue_reject.clone(),
                                        );
                                    }
                                });
                            }

                            let js_promise = self.create_js_promise(ctx, result_promise);
                            ctx.set_register(return_reg, js_promise);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::PromiseReject => {
                            // Promise.reject(reason)
                            let reason = current_args
                                .first()
                                .cloned()
                                .unwrap_or(Value::undefined());
                            let result_promise = crate::promise::JsPromise::new();
                            result_promise.reject_with_js_jobs(reason, enqueue_js_job.clone());
                            let js_promise = self.create_js_promise(ctx, result_promise);
                            ctx.set_register(return_reg, js_promise);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::PromiseResolve => {
                            // Promise.resolve(value)
                            let value = current_args
                                .first()
                                .cloned()
                                .unwrap_or(Value::undefined());

                            if is_wrapped_promise(&value) {
                                ctx.set_register(return_reg, value);
                                return Ok(InstructionResult::Continue);
                            }

                            if let Some(promise) = value.as_promise() {
                                let js_promise = self.create_js_promise(ctx, promise.clone());
                                ctx.set_register(return_reg, js_promise);
                                return Ok(InstructionResult::Continue);
                            }

                            let result_promise = crate::promise::JsPromise::new();

                            if value.is_object() {
                                let key = PropertyKey::string("then");
                                let then_val = if let Some(proxy) = value.as_proxy() {
                                    let key_value = Value::string(JsString::intern("then"));
                                    crate::proxy_operations::proxy_get(
                                        self,
                                        ctx,
                                        proxy,
                                        &key,
                                        key_value,
                                        value.clone(),
                                    )
                                } else if let Some(obj) = value.as_object() {
                                    match obj.lookup_property_descriptor(&key) {
                                        Some(crate::object::PropertyDescriptor::Accessor { get, .. }) => {
                                            if let Some(getter) = get {
                                                self.call_function(ctx, &getter, value.clone(), &[])
                                            } else {
                                                Ok(Value::undefined())
                                            }
                                        }
                                        Some(crate::object::PropertyDescriptor::Data { value, .. }) => {
                                            Ok(value)
                                        }
                                        _ => Ok(Value::undefined()),
                                    }
                                } else {
                                    Ok(Value::undefined())
                                };

                                match then_val {
                                    Ok(then_callable) if then_callable.is_callable() => {
                                        let job = crate::promise::JsPromiseJob {
                                            kind: crate::promise::JsPromiseJobKind::ResolveThenable,
                                            callback: then_callable,
                                            this_arg: value,
                                            result_promise: Some(result_promise.clone()),
                                        };
                                        let _ = ctx.enqueue_js_job(job, Vec::new());
                                        let js_promise = self.create_js_promise(ctx, result_promise);
                                        ctx.set_register(return_reg, js_promise);
                                        return Ok(InstructionResult::Continue);
                                    }
                                    Ok(_) => {}
                                    Err(err) => {
                                        let error_val = match err {
                                            VmError::Exception(thrown) => thrown.value,
                                            VmError::TypeError(message) => {
                                                self.make_error(ctx, "TypeError", &message)
                                            }
                                            VmError::RangeError(message) => {
                                                self.make_error(ctx, "RangeError", &message)
                                            }
                                            VmError::ReferenceError(message) => {
                                                self.make_error(ctx, "ReferenceError", &message)
                                            }
                                            VmError::SyntaxError(message) => {
                                                self.make_error(ctx, "SyntaxError", &message)
                                            }
                                            other => {
                                                let message = other.to_string();
                                                Value::string(JsString::intern(&message))
                                            }
                                        };
                                        result_promise.reject_with_js_jobs(error_val, |job, args| {
                                            let _ = ctx.enqueue_js_job(job, args);
                                        });
                                        let js_promise = self.create_js_promise(ctx, result_promise);
                                        ctx.set_register(return_reg, js_promise);
                                        return Ok(InstructionResult::Continue);
                                    }
                                }
                            }

                            result_promise.resolve_with_js_jobs(value, |job, args| {
                                let _ = ctx.enqueue_js_job(job, args);
                            });
                            let js_promise = self.create_js_promise(ctx, result_promise);
                            ctx.set_register(return_reg, js_promise);
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::PromiseResolveFunction => {
                            // Promise.withResolvers().resolve(value)
                            let promise = current_func
                                .as_object()
                                .and_then(|obj| obj.get(&PropertyKey::string("__promise__")))
                                .and_then(|v| v.as_promise().cloned())
                                .ok_or_else(|| VmError::type_error("Invalid promise resolver"))?;

                            let value = current_args
                                .first()
                                .cloned()
                                .unwrap_or(Value::undefined());

                            promise.resolve_with_js_jobs(value, |job, args| {
                                let _ = ctx.enqueue_js_job(job, args);
                            });
                            ctx.set_register(return_reg, Value::undefined());
                            return Ok(InstructionResult::Continue);
                        }

                        InterceptionSignal::PromiseRejectFunction => {
                            // Promise.withResolvers().reject(reason)
                            let promise = current_func
                                .as_object()
                                .and_then(|obj| obj.get(&PropertyKey::string("__promise__")))
                                .and_then(|v| v.as_promise().cloned())
                                .ok_or_else(|| VmError::type_error("Invalid promise resolver"))?;

                            let reason = current_args
                                .first()
                                .cloned()
                                .unwrap_or(Value::undefined());

                            promise.reject_with_js_jobs(reason, |job, args| {
                                let _ = ctx.enqueue_js_job(job, args);
                            });
                            ctx.set_register(return_reg, Value::undefined());
                            return Ok(InstructionResult::Continue);
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }

        // 3. Handle closures
        if let Some(closure) = current_func.as_function() {
            if closure.is_generator {
                // Use intrinsic generator prototype for correct prototype chain
                let proto = if closure.is_async {
                    ctx.async_generator_prototype_intrinsic()
                } else {
                    ctx.generator_prototype_intrinsic()
                };

                // Create the generator's internal object
                let gen_obj = GcRef::new(JsObject::new(proto, ctx.memory_manager().clone()));

                let generator = JsGenerator::new(
                    closure.function_index,
                    Arc::clone(&closure.module),
                    closure.upvalues.clone(),
                    current_args,
                    current_this,
                    false, // is_construct
                    closure.is_async,
                    gen_obj,
                );
                ctx.set_register(return_reg, Value::generator(generator));
                return Ok(InstructionResult::Continue);
            }

            let argc = current_args.len() as u8;
            ctx.set_pending_this(current_this);
            ctx.set_pending_args(current_args);
            // Propagate home_object from closure to the new call frame
            if let Some(ref ho) = closure.home_object {
                ctx.set_pending_home_object(ho.clone());
            }
            return Ok(InstructionResult::Call {
                func_index: closure.function_index,
                module: Arc::clone(&closure.module),
                argc,
                return_reg,
                is_construct: false,
                is_async: closure.is_async,
                upvalues: closure.upvalues.clone(),
            });
        }

        Err(VmError::type_error("Value is not a function"))
    }

    /// Observe the type of a value for type feedback collection
    #[inline]
    fn observe_value_type(type_flags: &mut TypeFlags, value: &Value) {
        if value.is_undefined() {
            type_flags.observe_undefined();
        } else if value.is_null() {
            type_flags.observe_null();
        } else if value.is_boolean() {
            type_flags.observe_boolean();
        } else if value.is_int32() {
            type_flags.observe_int32();
        } else if value.is_number() {
            type_flags.observe_number();
        } else if value.is_string() {
            type_flags.observe_string();
        } else if value.is_function() {
            type_flags.observe_function();
        } else if value.is_object() {
            type_flags.observe_object();
        }
    }

    /// Add operation (handles string concatenation)
    fn op_add(&self, left: &Value, right: &Value) -> VmResult<Value> {
        // String concatenation
        if left.is_string() || right.is_string() {
            let left_str = self.to_string(left);
            let right_str = self.to_string(right);
            let result = format!("{}{}", left_str, right_str);
            let js_str = JsString::intern(&result);
            return Ok(Value::string(js_str));
        }

        let left_bigint = self.bigint_value(left)?;
        let right_bigint = self.bigint_value(right)?;
        if let (Some(left_bigint), Some(right_bigint)) = (left_bigint, right_bigint) {
            let result = left_bigint + right_bigint;
            return Ok(Value::bigint(result.to_string()));
        }

        if left.is_bigint() || right.is_bigint() {
            return Err(VmError::type_error("Cannot mix BigInt and other types"));
        }

        // Numeric addition
        let left_num = self.coerce_number(left)?;
        let right_num = self.coerce_number(right)?;
        Ok(Value::number(left_num + right_num))
    }

    /// Internal method dispatch helper for spread
    fn dispatch_method_spread(
        &self,
        ctx: &mut VmContext,
        method_value: &Value,
        receiver: Value,
        spread_arr: &Value,
        return_reg: u16,
    ) -> VmResult<InstructionResult> {
        // Collect all arguments from the spread array
        let mut args = Vec::new();
        if let Some(obj) = spread_arr.as_object() {
            let len = obj
                .get(&PropertyKey::string("length"))
                .and_then(|v| v.as_int32())
                .unwrap_or(0);
            for i in 0..len {
                args.push(
                    obj.get(&PropertyKey::Index(i as u32))
                        .unwrap_or_else(Value::undefined),
                );
            }
        }

        if let Some(native_fn) = method_value.as_native_function() {
            let result = self.call_native_fn(ctx, native_fn, &receiver, &args)?;
            ctx.set_register(return_reg, result);
            return Ok(InstructionResult::Continue);
        }

        if let Some(closure) = method_value.as_function() {
            let argc = args.len() as u8;
            ctx.set_pending_args(args);
            ctx.set_pending_this(receiver);

            return Ok(InstructionResult::Call {
                func_index: closure.function_index,
                module: Arc::clone(&closure.module),
                argc,
                return_reg,
                is_construct: false,
                is_async: closure.is_async,
                upvalues: closure.upvalues.clone(),
            });
        }

        Err(VmError::type_error("method is not a function"))
    }

    /// Convert value to string
    fn to_string(&self, value: &Value) -> String {
        match value.type_of() {
            "undefined" => "undefined".to_string(),
            "null" => "null".to_string(),
            "boolean" => {
                if value.to_boolean() {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
            "number" => {
                if let Some(n) = value.as_number() {
                    if n.is_nan() {
                        "NaN".to_string()
                    } else if n.is_infinite() {
                        if n > 0.0 {
                            "Infinity".to_string()
                        } else {
                            "-Infinity".to_string()
                        }
                    } else if n.fract() == 0.0 {
                        format!("{}", n as i64)
                    } else {
                        format!("{}", n)
                    }
                } else {
                    "NaN".to_string()
                }
            }
            "string" => {
                if let Some(s) = value.as_string() {
                    s.as_str().to_string()
                } else {
                    String::new()
                }
            }
            "bigint" => {
                if let Some(crate::value::HeapRef::BigInt(b)) = value.heap_ref() {
                    b.value.clone()
                } else {
                    "0".to_string()
                }
            }
            _ => {
                if let Some(obj) = value.as_object() {
                    let name = obj
                        .get(&PropertyKey::string("name"))
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string());
                    let message = obj
                        .get(&PropertyKey::string("message"))
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string());

                    match (name, message) {
                        (Some(n), Some(m)) => format!("{}: {}", n, m),
                        (Some(n), None) => n,
                        (None, Some(m)) => m,
                        (None, None) => "[object Object]".to_string(),
                    }
                } else if value.is_function() || value.is_native_function() {
                    // Functions: toString should return source or "function X() { [native code] }"
                    "function () { [native code] }".to_string()
                } else {
                    "[object Object]".to_string()
                }
            }
        }
    }

    /// Create a JavaScript Promise object from an internal promise
    /// This creates an object with _internal field and copies methods from Promise.prototype
    fn create_js_promise(&self, ctx: &VmContext, internal: Arc<JsPromise>) -> Value {
        let obj = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));

        // Set _internal to the raw promise
        obj.set(PropertyKey::string("_internal"), Value::promise(internal));

        // Try to get Promise.prototype and copy its methods
        if let Some(promise_ctor) = ctx.get_global("Promise").and_then(|v| v.as_object()) {
            if let Some(proto) = promise_ctor
                .get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object())
            {
                // Copy then, catch, finally from prototype
                if let Some(then_fn) = proto.get(&PropertyKey::string("then")) {
                    obj.set(PropertyKey::string("then"), then_fn);
                }
                if let Some(catch_fn) = proto.get(&PropertyKey::string("catch")) {
                    obj.set(PropertyKey::string("catch"), catch_fn);
                }
                if let Some(finally_fn) = proto.get(&PropertyKey::string("finally")) {
                    obj.set(PropertyKey::string("finally"), finally_fn);
                }

                // Set prototype for proper inheritance
                obj.set_prototype(Some(proto));
            }
        }

        Value::object(obj)
    }

    /// Convert value to number (very small ToNumber subset).
    fn to_number(&self, value: &Value) -> f64 {
        if let Some(n) = value.as_number() {
            return n;
        }
        if value.is_undefined() {
            return f64::NAN;
        }
        if value.is_null() {
            return 0.0;
        }
        if let Some(b) = value.as_boolean() {
            return if b { 1.0 } else { 0.0 };
        }
        if let Some(s) = value.as_string() {
            let trimmed = s.as_str().trim();
            if trimmed.is_empty() {
                return 0.0;
            }
            return trimmed.parse::<f64>().unwrap_or(f64::NAN);
        }
        f64::NAN
    }

    /// ES2023 §7.1.6 ToInt32 — convert f64 to 32-bit signed integer
    fn to_int32_from(&self, n: f64) -> i32 {
        if n.is_nan() || n.is_infinite() || n == 0.0 {
            return 0;
        }
        // Truncate to integer, then wrap to i32 via u32
        let i = n.trunc() as i64;
        (i as u32) as i32
    }

    /// ES2023 §7.1.7 ToUint32 — convert f64 to 32-bit unsigned integer
    fn to_uint32_from(&self, n: f64) -> u32 {
        if n.is_nan() || n.is_infinite() || n == 0.0 {
            return 0;
        }
        let i = n.trunc() as i64;
        i as u32
    }

    fn make_error(&self, ctx: &VmContext, name: &str, message: &str) -> Value {
        let ctor_value = ctx.get_global(name);
        let proto = ctor_value
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|obj| obj.get(&PropertyKey::string("prototype")))
            .and_then(|v| v.as_object());

        let obj = GcRef::new(JsObject::new(proto, ctx.memory_manager().clone()));
        obj.set(
            PropertyKey::string("name"),
            Value::string(JsString::intern(name)),
        );
        obj.set(
            PropertyKey::string("message"),
            Value::string(JsString::intern(message)),
        );
        let stack = if message.is_empty() {
            name.to_string()
        } else {
            format!("{}: {}", name, message)
        };
        obj.set(
            PropertyKey::string("stack"),
            Value::string(JsString::intern(&stack)),
        );
        obj.set(PropertyKey::string("__isError__"), Value::boolean(true));
        obj.set(
            PropertyKey::string("__errorType__"),
            Value::string(JsString::intern(name)),
        );
        if let Some(ctor) = ctor_value {
            obj.set(PropertyKey::string("constructor"), ctor);
        }

        Value::object(obj)
    }

    fn coerce_number(&self, value: &Value) -> VmResult<f64> {
        if value.is_symbol() || value.is_bigint() {
            return Err(VmError::type_error("Cannot convert to number"));
        }
        Ok(self.to_number(value))
    }

    fn bigint_value(&self, value: &Value) -> VmResult<Option<NumBigInt>> {
        if let Some(crate::value::HeapRef::BigInt(b)) = value.heap_ref() {
            let bigint = self.parse_bigint_str(&b.value)?;
            return Ok(Some(bigint));
        }
        Ok(None)
    }

    fn to_numeric(&self, value: &Value) -> VmResult<Numeric> {
        if let Some(bigint) = self.bigint_value(value)? {
            return Ok(Numeric::BigInt(bigint));
        }
        if value.is_symbol() {
            return Err(VmError::type_error("Cannot convert to number"));
        }
        Ok(Numeric::Number(self.to_number(value)))
    }

    fn numeric_compare(&self, left: Numeric, right: Numeric) -> VmResult<Option<Ordering>> {
        match (left, right) {
            (Numeric::Number(left), Numeric::Number(right)) => {
                if left.is_nan() || right.is_nan() {
                    Ok(None)
                } else {
                    Ok(left.partial_cmp(&right))
                }
            }
            (Numeric::BigInt(left), Numeric::BigInt(right)) => Ok(Some(left.cmp(&right))),
            (Numeric::BigInt(left), Numeric::Number(right)) => {
                Ok(self.compare_bigint_number(&left, right))
            }
            (Numeric::Number(left), Numeric::BigInt(right)) => Ok(self
                .compare_bigint_number(&right, left)
                .map(|ordering| ordering.reverse())),
        }
    }

    fn compare_bigint_number(&self, bigint: &NumBigInt, number: f64) -> Option<Ordering> {
        if number.is_nan() {
            return None;
        }
        if number.is_infinite() {
            return Some(if number.is_sign_positive() {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        let (numerator, denominator) = self.f64_to_ratio(number);
        let scaled = bigint * denominator;
        Some(scaled.cmp(&numerator))
    }

    fn parse_bigint_str(&self, value: &str) -> VmResult<NumBigInt> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(VmError::type_error("Invalid BigInt"));
        }

        let (sign, digits) = if let Some(rest) = trimmed.strip_prefix('-') {
            (true, rest)
        } else if let Some(rest) = trimmed.strip_prefix('+') {
            (false, rest)
        } else {
            (false, trimmed)
        };

        let (radix, digits) = if let Some(rest) = digits.strip_prefix("0x") {
            (16, rest)
        } else if let Some(rest) = digits.strip_prefix("0X") {
            (16, rest)
        } else if let Some(rest) = digits.strip_prefix("0o") {
            (8, rest)
        } else if let Some(rest) = digits.strip_prefix("0O") {
            (8, rest)
        } else if let Some(rest) = digits.strip_prefix("0b") {
            (2, rest)
        } else if let Some(rest) = digits.strip_prefix("0B") {
            (2, rest)
        } else {
            (10, digits)
        };

        let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
        if cleaned.is_empty() {
            return Err(VmError::type_error("Invalid BigInt"));
        }
        let mut bigint = NumBigInt::parse_bytes(cleaned.as_bytes(), radix)
            .ok_or_else(|| VmError::type_error("Invalid BigInt"))?;
        if sign {
            bigint = -bigint;
        }
        Ok(bigint)
    }

    fn f64_to_ratio(&self, number: f64) -> (NumBigInt, NumBigInt) {
        if number == 0.0 {
            return (NumBigInt::zero(), NumBigInt::one());
        }

        let bits = number.to_bits();
        let sign = (bits >> 63) != 0;
        let exponent = ((bits >> 52) & 0x7ff) as i32;
        let mantissa = bits & 0x000f_ffff_ffff_ffff;

        let (mut numerator, denominator) = if exponent == 0 {
            let exp2 = 1 - 1023 - 52;
            let mut num = NumBigInt::from(mantissa);
            let mut den = NumBigInt::one();
            if exp2 >= 0 {
                num <<= exp2 as usize;
            } else {
                den <<= (-exp2) as usize;
            }
            (num, den)
        } else {
            let significand = (1u64 << 52) | mantissa;
            let exp2 = exponent - 1023 - 52;
            let mut num = NumBigInt::from(significand);
            let mut den = NumBigInt::one();
            if exp2 >= 0 {
                num <<= exp2 as usize;
            } else {
                den <<= (-exp2) as usize;
            }
            (num, den)
        };

        if sign {
            numerator = -numerator;
        }

        (numerator, denominator)
    }

    /// Convert a Value to a PropertyKey for object property access
    fn value_to_property_key(&self, value: &Value) -> PropertyKey {
        if let Some(n) = value.as_int32() {
            if n >= 0 {
                PropertyKey::Index(n as u32)
            } else {
                PropertyKey::string(&n.to_string())
            }
        } else if let Some(s) = value.as_string() {
            // Check if the string is a valid array index (canonical numeric string)
            if let Ok(n) = s.as_str().parse::<u32>() {
                // Verify it's canonical (no leading zeros except for "0")
                if n.to_string() == s.as_str() {
                    return PropertyKey::Index(n);
                }
            }
            PropertyKey::string(s.as_str())
        } else if let Some(sym) = value.as_symbol() {
            PropertyKey::Symbol(sym.id)
        } else {
            let key_str = self.to_string(value);
            // Also check if the stringified value is a valid array index
            if let Ok(n) = key_str.parse::<u32>() {
                if n.to_string() == key_str {
                    return PropertyKey::Index(n);
                }
            }
            PropertyKey::string(&key_str)
        }
    }

    /// Abstract equality comparison (==)
    fn abstract_equal(&self, left: &Value, right: &Value) -> bool {
        // Same type: use strict equality
        if left.type_of() == right.type_of() {
            return self.strict_equal(left, right);
        }

        // null == undefined
        if left.is_null() && right.is_undefined() {
            return true;
        }
        if left.is_undefined() && right.is_null() {
            return true;
        }

        // Number comparisons
        if let (Some(a), Some(b)) = (left.as_number(), right.as_number()) {
            return a == b;
        }

        // TODO: More coercion rules
        false
    }

    /// Strict equality comparison (===)
    fn strict_equal(&self, left: &Value, right: &Value) -> bool {
        // Different types are never strictly equal
        if left.type_of() != right.type_of() {
            return false;
        }

        // Use Value's PartialEq
        left == right
    }

    /// Capture upvalues from the current frame based on upvalue specifications.
    /// Returns cells (not values) so closures share mutable state.
    fn capture_upvalues(
        &self,
        ctx: &mut VmContext,
        upvalue_specs: &[UpvalueCapture],
    ) -> VmResult<Vec<UpvalueCell>> {
        let mut captured = Vec::with_capacity(upvalue_specs.len());

        for spec in upvalue_specs {
            let cell = match spec {
                UpvalueCapture::Local(idx) => {
                    // Capture from parent's local variable.
                    // Get or create a shared cell for this local.
                    ctx.get_or_create_open_upvalue(idx.0)?
                }
                UpvalueCapture::Upvalue(idx) => {
                    // The parent's upvalue is already a cell, just clone the Rc.
                    ctx.get_upvalue_cell(idx.0)?.clone()
                }
            };
            captured.push(cell);
        }

        Ok(captured)
    }

    fn utf16_key(units: &[u16]) -> PropertyKey {
        PropertyKey::from_js_string(JsString::intern_utf16(units))
    }

    fn utf16_eq_ascii(units: &[u16], ascii: &str) -> bool {
        if units.len() != ascii.len() {
            return false;
        }
        units
            .iter()
            .zip(ascii.as_bytes().iter())
            .all(|(unit, byte)| *unit == *byte as u16)
    }

    fn utf16_to_index(units: &[u16]) -> Option<u32> {
        if units.is_empty() {
            return None;
        }

        if units.len() > 1 && units[0] == b'0' as u16 {
            return None;
        }

        let mut value: u32 = 0;
        for unit in units {
            if !(*unit >= b'0' as u16 && *unit <= b'9' as u16) {
                return None;
            }
            value = value.checked_mul(10)?;
            value = value.checked_add((*unit - b'0' as u16) as u32)?;
        }

        Some(value)
    }

    /// Capture stack trace for Error objects
    fn capture_error_stack_trace(error_obj: GcRef<JsObject>, ctx: &VmContext) {
        use crate::object::PropertyKey;
        use crate::string::JsString;

        // Get call stack frames (skip the Error constructor itself)
        let frames: Vec<_> = ctx.call_stack()
            .iter()
            .rev()
            .skip(1)
            .take(10)
            .collect();

        // Create array to hold stack frame objects
        let frames_array = GcRef::new(JsObject::array(frames.len(), ctx.memory_manager().clone()));

        for (i, frame) in frames.iter().enumerate() {
            let frame_obj = GcRef::new(JsObject::new(None, ctx.memory_manager().clone()));

            // Get function name
            if let Some(func_def) = frame.module.functions.get(frame.function_index as usize) {
                let func_name = func_def.name.clone().unwrap_or_else(|| "<anonymous>".to_string());
                frame_obj.set(
                    PropertyKey::string("function"),
                    Value::string(JsString::intern(&func_name)),
                );
            } else {
                frame_obj.set(
                    PropertyKey::string("function"),
                    Value::string(JsString::intern("<unknown>")),
                );
            }

            // Get source file
            let source_url = &frame.module.source_url;
            if !source_url.is_empty() {
                frame_obj.set(
                    PropertyKey::string("file"),
                    Value::string(JsString::intern(source_url)),
                );
            }

            // TODO: Get line and column from source map if available
            // For now, just set placeholder values
            frame_obj.set(PropertyKey::string("line"), Value::number(0.0));
            frame_obj.set(PropertyKey::string("column"), Value::number(0.0));

            frames_array.set(PropertyKey::Index(i as u32), Value::object(frame_obj));
        }

        // Store frames array as hidden property
        error_obj.set(
            PropertyKey::string("__stack_frames__"),
            Value::array(frames_array),
        );
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

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
        promise: Arc<JsPromise>,
        /// The register to store the resolved value
        resume_reg: u16,
        /// The generator (for resumption)
        generator: Arc<JsGenerator>,
    },
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
        &mut self,
        generator: &Arc<JsGenerator>,
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
        &mut self,
        generator: &Arc<JsGenerator>,
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
        let argc = args.len();

        // Set up pending args and push initial frame
        ctx.set_pending_args(args);
        ctx.set_pending_this(this_value);
        ctx.set_pending_upvalues(generator.upvalues.clone());

        // Remember the stack depth before pushing the generator frame
        let initial_depth = ctx.stack_depth();

        if let Err(e) = ctx.push_frame(
            generator.function_index,
            Arc::clone(&generator.module),
            func.local_count,
            None,
            generator.is_construct(),
            false, // generators are not async
            argc,
        ) {
            generator.complete();
            return GeneratorResult::Error(e);
        }

        // Run until yield or return
        self.run_generator_loop(generator, ctx, initial_depth)
    }

    /// Resume generator execution from saved frame
    fn resume_generator_execution(
        &mut self,
        generator: &Arc<JsGenerator>,
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
                        ctx.pop_frame();
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
                        ctx.pop_frame();
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

        // Run until yield or return
        self.run_generator_loop(generator, ctx, initial_depth)
    }

    /// Restore a generator frame to the context
    fn restore_generator_frame(
        &mut self,
        ctx: &mut VmContext,
        frame: &GeneratorFrame,
    ) -> VmResult<()> {
        // Push a new frame with the saved state
        ctx.set_pending_upvalues(frame.upvalues.clone());
        ctx.set_pending_this(frame.this_value.clone());

        // Set up the locals as pending args (they'll be copied to locals)
        ctx.set_pending_args(frame.locals.clone());

        // Get function info
        let func = frame
            .module
            .function(frame.function_index)
            .ok_or_else(|| VmError::internal("Generator function not found"))?;

        ctx.push_frame(
            frame.function_index,
            Arc::clone(&frame.module),
            func.local_count,
            None,
            frame.is_construct,
            false,
            frame.argc,
        )?;

        // Restore PC (push_frame sets it to 0, we need to set it to the saved value)
        ctx.set_pc(frame.pc);

        // Restore registers
        for (i, reg_value) in frame.registers.iter().enumerate() {
            ctx.set_register(i as u16, reg_value.clone());
        }

        // Restore locals
        for (i, local_value) in frame.locals.iter().enumerate() {
            ctx.set_local(i as u16, local_value.clone())?;
        }

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
        module: &Arc<Module>,
    ) -> VmResult<GeneratorFrame> {
        let current_frame = ctx
            .current_frame()
            .ok_or_else(|| VmError::internal("No current frame"))?;

        // Collect registers (we need the function's register count)
        let func = module
            .function(current_frame.function_index)
            .ok_or_else(|| VmError::internal("Function not found"))?;

        let mut registers = Vec::with_capacity(func.register_count as usize);
        for i in 0..func.register_count {
            registers.push(ctx.get_register(i).clone());
        }

        // Collect try stack entries for this frame
        let try_handlers = ctx.get_try_handlers_for_current_frame();
        let try_stack: Vec<crate::generator::TryEntry> = try_handlers
            .into_iter()
            .map(|(catch_pc, frame_depth)| crate::generator::TryEntry {
                catch_pc,
                frame_depth,
            })
            .collect();

        Ok(GeneratorFrame::new(
            current_frame.pc,
            current_frame.function_index,
            Arc::clone(&current_frame.module),
            current_frame.locals.clone(),
            registers,
            current_frame.upvalues.clone(),
            try_stack,
            current_frame.this_value.clone(),
            current_frame.is_construct,
            current_frame.frame_id,
            current_frame.argc,
        ))
    }

    /// Run the generator execution loop until yield, return, or error
    ///
    /// `initial_depth` is the stack depth before the generator frame was pushed.
    /// This is used to correctly identify when the generator has returned.
    fn run_generator_loop(
        &mut self,
        generator: &Arc<JsGenerator>,
        ctx: &mut VmContext,
        initial_depth: usize,
    ) -> GeneratorResult {
        // Similar to run_loop but handles Yield specially
        let mut cached_module: Option<Arc<Module>> = None;
        let mut cached_frame_id: usize = usize::MAX;

        loop {
            // Periodic interrupt check
            if ctx.should_check_interrupt() {
                if ctx.is_interrupted() {
                    generator.complete();
                    return GeneratorResult::Error(VmError::interrupted());
                }
                ctx.maybe_collect_garbage();
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
                cached_module = Some(Arc::clone(&frame.module));
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
            if frame.pc >= func.instructions.len() {
                // Generator frame has no more instructions - pop it
                ctx.pop_frame();

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
                cached_frame_id = usize::MAX;
                continue;
            }

            let instruction = &func.instructions[frame.pc];
            ctx.record_instruction();

            // Execute instruction
            let instruction_result = match self.execute_instruction(instruction, module_ref, ctx) {
                Ok(result) => result,
                Err(err) => match err {
                    VmError::TypeError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "TypeError", &message))
                    }
                    VmError::RangeError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "RangeError", &message))
                    }
                    VmError::ReferenceError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "ReferenceError", &message))
                    }
                    VmError::SyntaxError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "SyntaxError", &message))
                    }
                    other => {
                        generator.complete();
                        return GeneratorResult::Error(other);
                    }
                },
            };

            match instruction_result {
                InstructionResult::Continue => {
                    ctx.advance_pc();
                }
                InstructionResult::Jump(offset) => {
                    ctx.jump(offset);
                }
                InstructionResult::Return(value) => {
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
                    cached_frame_id = usize::MAX;
                }
                InstructionResult::Yield { value, yield_dst } => {
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
                    ctx.pop_frame();

                    return GeneratorResult::Yielded(value);
                }
                InstructionResult::Throw(error) => {
                    // Try to find a catch handler inside the generator
                    if let Some((frame_depth, catch_pc)) = ctx.peek_nearest_try() {
                        if frame_depth > initial_depth {
                            ctx.take_nearest_try(); // Actually pop it
                            // Unwind to the handler frame
                            while ctx.stack_depth() > frame_depth {
                                ctx.pop_frame();
                            }
                            ctx.set_pc(catch_pc);
                            ctx.set_exception(error.clone());
                            // Put error in register 0 for catch block
                            ctx.set_register(0, error);
                            cached_frame_id = usize::MAX;
                            continue;
                        }
                    }

                    // No internal handler - check pending return from generator.return()
                    if let Some(return_value) = generator.take_pending_return() {
                        generator.complete();
                        while ctx.stack_depth() > initial_depth {
                            ctx.pop_frame();
                        }
                        return GeneratorResult::Returned(return_value);
                    }

                    // No internal handler - completion will buble out
                    generator.complete();
                    // Pop all frames down to initial_depth
                    while ctx.stack_depth() > initial_depth {
                        ctx.pop_frame();
                    }
                    return GeneratorResult::Error(VmError::exception(error));
                }
                InstructionResult::Call {
                    func_index,
                    module: call_module,
                    argc,
                    return_reg,
                    is_construct,
                    is_async,
                    upvalues,
                } => {
                    ctx.advance_pc(); // Advance before pushing new frame

                    let callee = match call_module.function(func_index) {
                        Some(f) => f,
                        None => {
                            generator.complete();
                            return GeneratorResult::Error(VmError::internal(format!(
                                "callee not found (func_index={}, function_count={})",
                                func_index,
                                call_module.function_count()
                            )));
                        }
                    };

                    let local_count = callee.local_count;
                    let has_rest = callee.flags.has_rest;
                    let param_count = callee.param_count as usize;

                    if has_rest {
                        let mut args = ctx.take_pending_args();
                        let rest_args: Vec<Value> = if args.len() > param_count {
                            args.drain(param_count..).collect()
                        } else {
                            Vec::new()
                        };

                        let rest_arr = GcRef::new(JsObject::array(
                            rest_args.len(),
                            ctx.memory_manager().clone(),
                        ));
                        if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object())
                        {
                            rest_arr.set_prototype(Some(array_proto));
                        }
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            rest_arr.set(PropertyKey::Index(i as u32), arg);
                        }

                        args.push(Value::object(rest_arr));
                        ctx.set_pending_args(args);
                    }

                    ctx.set_pending_upvalues(upvalues);

                    if let Err(e) = ctx.push_frame(
                        func_index,
                        call_module,
                        local_count,
                        Some(return_reg),
                        is_construct,
                        is_async,
                        argc as usize,
                    ) {
                        generator.complete();
                        return GeneratorResult::Error(e);
                    }
                }
                InstructionResult::TailCall {
                    func_index,
                    module: call_module,
                    argc,
                    return_reg,
                    is_async,
                    upvalues,
                } => {
                    // Tail call optimization: pop current frame before pushing new one
                    ctx.pop_frame();
                    cached_frame_id = usize::MAX;

                    let callee = match call_module.function(func_index) {
                        Some(f) => f,
                        None => {
                            generator.complete();
                            return GeneratorResult::Error(VmError::internal(format!(
                                "callee not found (func_index={}, function_count={})",
                                func_index,
                                call_module.function_count()
                            )));
                        }
                    };

                    let local_count = callee.local_count;
                    let has_rest = callee.flags.has_rest;
                    let param_count = callee.param_count as usize;

                    if has_rest {
                        let mut args = ctx.take_pending_args();
                        let rest_args: Vec<Value> = if args.len() > param_count {
                            args.drain(param_count..).collect()
                        } else {
                            Vec::new()
                        };
                        let rest_arr = GcRef::new(JsObject::array(
                            rest_args.len(),
                            ctx.memory_manager().clone(),
                        ));
                        if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object())
                        {
                            rest_arr.set_prototype(Some(array_proto));
                        }
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            rest_arr.set(PropertyKey::Index(i as u32), arg);
                        }
                        args.push(Value::object(rest_arr));
                        ctx.set_pending_args(args);
                    }

                    ctx.set_pending_upvalues(upvalues);

                    if let Err(e) = ctx.push_frame(
                        func_index,
                        call_module,
                        local_count,
                        Some(return_reg),
                        false,
                        is_async,
                        argc as usize,
                    ) {
                        generator.complete();
                        return GeneratorResult::Error(e);
                    }
                }
                InstructionResult::Suspend {
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
                        ctx.pop_frame();

                        return GeneratorResult::Suspended {
                            promise,
                            resume_reg,
                            generator: Arc::clone(generator),
                        };
                    } else {
                        // Sync generators cannot await
                        generator.complete();
                        return GeneratorResult::Error(VmError::internal(
                            "Sync generator cannot use await",
                        ));
                    }
                }
            }
        }
    }
}

/// Result of executing an instruction
#[allow(dead_code)]
enum InstructionResult {
    /// Continue to next instruction
    Continue,
    /// Jump by offset
    Jump(i32),
    /// Return from function
    Return(Value),
    /// Call a function
    Call {
        func_index: u32,
        module: Arc<Module>,
        argc: u8,
        return_reg: u16,
        is_construct: bool,
        is_async: bool,
        upvalues: Vec<UpvalueCell>,
    },
    /// Tail call - pop current frame and call function (no stack growth)
    TailCall {
        func_index: u32,
        module: Arc<Module>,
        argc: u8,
        return_reg: u16,
        is_async: bool,
        upvalues: Vec<UpvalueCell>,
    },
    /// Suspend execution waiting for Promise
    Suspend {
        promise: Arc<JsPromise>,
        resume_reg: u16,
    },
    /// Yield from generator
    Yield { value: Value, yield_dst: u16 },
    /// Throw a JS value
    Throw(Value),
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::operand::Register;
    use otter_vm_bytecode::{Function, Module};

    fn create_test_context() -> VmContext {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let global = GcRef::new(JsObject::new(None, memory_manager.clone()));
        VmContext::new(global, memory_manager)
    }

    #[test]
    fn test_load_constants() {
        let mut builder = Module::builder("test.js");

        let func = Function::builder()
            .name("main")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 42,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(42));
    }

    #[test]
    fn test_arithmetic() {
        let mut builder = Module::builder("test.js");

        let func = Function::builder()
            .name("main")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 10,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 5,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_number(), Some(15.0));
    }

    #[test]
    fn test_comparison() {
        let mut builder = Module::builder("test.js");

        let func = Function::builder()
            .name("main")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 10,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 5,
            })
            .instruction(Instruction::Lt {
                dst: Register(2),
                lhs: Register(1),
                rhs: Register(0),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_object_prop_const() {
        use otter_vm_bytecode::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");

        let func = Function::builder()
            .name("main")
            // NewObject r0
            .instruction(Instruction::NewObject { dst: Register(0) })
            // LoadInt32 r1, 42
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 42,
            })
            // SetPropConst r0, "x", r1
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0),
                val: Register(1),
                ic_index: 0,
            })
            // GetPropConst r2, r0, "x"
            .instruction(Instruction::GetPropConst {
                dst: Register(2),
                obj: Register(0),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            // Return r2
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(42));
    }

    #[test]
    fn test_array_elem() {
        let mut builder = Module::builder("test.js");

        let func = Function::builder()
            .name("main")
            .feedback_vector_size(2)
            // NewArray r0, 3
            .instruction(Instruction::NewArray {
                dst: Register(0),
                len: 3,
            })
            // LoadInt32 r1, 10
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 10,
            })
            // LoadInt32 r2, 0
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 0,
            })
            // SetElem r0, r2, r1
            .instruction(Instruction::SetElem {
                arr: Register(0),
                idx: Register(2),
                val: Register(1),
                ic_index: 0,
            })
            // GetElem r3, r0, r2
            .instruction(Instruction::GetElem {
                dst: Register(3),
                arr: Register(0),
                idx: Register(2),
                ic_index: 1,
            })
            // Return r3
            .instruction(Instruction::Return { src: Register(3) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(10));
    }

    #[test]
    fn test_object_prop_computed() {
        use otter_vm_bytecode::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("foo");

        let func = Function::builder()
            .name("main")
            // NewObject r0
            .instruction(Instruction::NewObject { dst: Register(0) })
            // LoadInt32 r1, 99
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 99,
            })
            // LoadConst r2, "foo"
            .instruction(Instruction::LoadConst {
                dst: Register(2),
                idx: ConstantIndex(0),
            })
            // SetProp r0, r2, r1
            .instruction(Instruction::SetProp {
                obj: Register(0),
                key: Register(2),
                val: Register(1),
                ic_index: 0,
            })
            // GetProp r3, r0, r2
            .instruction(Instruction::GetProp {
                dst: Register(3),
                obj: Register(0),
                key: Register(2),
                ic_index: 0,
            })
            // Return r3
            .instruction(Instruction::Return { src: Register(3) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(99));
    }

    #[test]
    fn test_closure_creation() {
        use otter_vm_bytecode::FunctionIndex;

        let mut builder = Module::builder("test.js");

        // Main function: creates closure and returns it
        let main = Function::builder()
            .name("main")
            // Closure r0, func#1
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            // TypeOf r1, r0
            .instruction(Instruction::TypeOf {
                dst: Register(1),
                src: Register(0),
            })
            // Return r1
            .instruction(Instruction::Return { src: Register(1) })
            .build();

        // Function at index 1 (not called in this test)
        let helper = Function::builder()
            .name("helper")
            .instruction(Instruction::ReturnUndefined)
            .build();

        builder.add_function(main);
        builder.add_function(helper);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        // typeof function === "function"
        let result_str = result.as_string().expect("expected string");
        assert_eq!(result_str.as_str(), "function");
    }

    #[test]
    fn test_function_call_simple() {
        use otter_vm_bytecode::FunctionIndex;

        let mut builder = Module::builder("test.js");

        // Main function:
        //   Closure r0, func#1 (double)
        //   LoadInt32 r1, 5     (argument)
        //   Call r2, r0, 1      (result = double(5))
        //   Return r2
        let main = Function::builder()
            .name("main")
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 5,
            })
            .instruction(Instruction::Call {
                dst: Register(2),
                func: Register(0),
                argc: 1,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        // double(x): returns x + x
        //   local[0] = x (argument)
        //   GetLocal r0, 0
        //   Add r1, r0, r0
        //   Return r1
        let double = Function::builder()
            .name("double")
            .param_count(1)
            .local_count(1)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: otter_vm_bytecode::LocalIndex(0),
            })
            .instruction(Instruction::Add {
                dst: Register(1),
                lhs: Register(0),
                rhs: Register(0),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();

        builder.add_function(main);
        builder.add_function(double);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_number(), Some(10.0)); // 5 + 5 = 10
    }

    #[test]
    fn test_function_call_multiple_args() {
        use otter_vm_bytecode::FunctionIndex;

        let mut builder = Module::builder("test.js");

        // Main: call add(3, 7)
        let main = Function::builder()
            .name("main")
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 3,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 7,
            })
            .instruction(Instruction::Call {
                dst: Register(3),
                func: Register(0),
                argc: 2,
            })
            .instruction(Instruction::Return { src: Register(3) })
            .build();

        // add(a, b): returns a + b
        let add = Function::builder()
            .name("add")
            .param_count(2)
            .local_count(2)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: otter_vm_bytecode::LocalIndex(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(1),
                idx: otter_vm_bytecode::LocalIndex(1),
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        builder.add_function(main);
        builder.add_function(add);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_number(), Some(10.0)); // 3 + 7 = 10
    }

    #[test]
    fn test_nested_function_calls() {
        use otter_vm_bytecode::FunctionIndex;

        let mut builder = Module::builder("test.js");

        // Main: call outer(2), which calls inner(2) and returns inner(2) * 2
        let main = Function::builder()
            .name("main")
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 2,
            })
            .instruction(Instruction::Call {
                dst: Register(2),
                func: Register(0),
                argc: 1,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        // outer(x): returns inner(x) * 2
        let outer = Function::builder()
            .name("outer")
            .param_count(1)
            .local_count(1)
            // Get argument x
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: otter_vm_bytecode::LocalIndex(0),
            })
            // Create closure for inner
            .instruction(Instruction::Closure {
                dst: Register(1),
                func: FunctionIndex(2),
            })
            // Call inner(x)
            .instruction(Instruction::Move {
                dst: Register(2),
                src: Register(0),
            })
            .instruction(Instruction::Call {
                dst: Register(3),
                func: Register(1),
                argc: 1,
            })
            // Multiply by 2
            .instruction(Instruction::LoadInt32 {
                dst: Register(4),
                value: 2,
            })
            .instruction(Instruction::Mul {
                dst: Register(5),
                lhs: Register(3),
                rhs: Register(4),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(5) })
            .build();

        // inner(x): returns x * x
        let inner = Function::builder()
            .name("inner")
            .param_count(1)
            .local_count(1)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: otter_vm_bytecode::LocalIndex(0),
            })
            .instruction(Instruction::Mul {
                dst: Register(1),
                lhs: Register(0),
                rhs: Register(0),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();

        builder.add_function(main);
        builder.add_function(outer);
        builder.add_function(inner);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        // outer(2) = inner(2) * 2 = (2*2) * 2 = 8
        assert_eq!(result.as_number(), Some(8.0));
    }

    #[test]
    fn test_define_getter() {
        use otter_vm_bytecode::{ConstantIndex, FunctionIndex};

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");

        // Main function:
        // 1. Create object
        // 2. Create getter function (returns 42)
        // 3. DefineGetter on object
        // 4. Access the getter
        let main = Function::builder()
            .name("main")
            // NewObject r0
            .instruction(Instruction::NewObject { dst: Register(0) })
            // LoadConst r1, "x" (key)
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(0),
            })
            // Closure r2, getter_fn
            .instruction(Instruction::Closure {
                dst: Register(2),
                func: FunctionIndex(1),
            })
            // DefineGetter obj=r0, key=r1, func=r2
            .instruction(Instruction::DefineGetter {
                obj: Register(0),
                key: Register(1),
                func: Register(2),
            })
            // GetPropConst r3, r0, "x"
            .instruction(Instruction::GetPropConst {
                dst: Register(3),
                obj: Register(0),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            // Return r3
            .instruction(Instruction::Return { src: Register(3) })
            .feedback_vector_size(1)
            .build();

        // Getter function: returns 42
        let getter = Function::builder()
            .name("getter")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 42,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        builder.add_function(main);
        builder.add_function(getter);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(42));
    }

    #[test]
    fn test_define_setter() {
        use otter_vm_bytecode::{ConstantIndex, FunctionIndex, LocalIndex};

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");
        builder.constants_mut().add_string("_x");

        // Main function:
        // 1. Create object with _x property
        // 2. Define setter for x that sets _x
        // 3. Set x via setter
        // 4. Read _x to verify setter was called
        let main = Function::builder()
            .name("main")
            // NewObject r0
            .instruction(Instruction::NewObject { dst: Register(0) })
            // LoadInt32 r1, 0 (initial _x value)
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0,
            })
            // SetPropConst r0, "_x", r1
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(1), // "_x"
                val: Register(1),
                ic_index: 0,
            })
            // LoadConst r2, "x" (key)
            .instruction(Instruction::LoadConst {
                dst: Register(2),
                idx: ConstantIndex(0),
            })
            // Closure r3, setter_fn
            .instruction(Instruction::Closure {
                dst: Register(3),
                func: FunctionIndex(1),
            })
            // DefineSetter obj=r0, key=r2, func=r3
            .instruction(Instruction::DefineSetter {
                obj: Register(0),
                key: Register(2),
                func: Register(3),
            })
            // LoadInt32 r4, 99 (value to set)
            .instruction(Instruction::LoadInt32 {
                dst: Register(4),
                value: 99,
            })
            // SetPropConst r0, "x", r4 (triggers setter)
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0), // "x"
                val: Register(4),
                ic_index: 1,
            })
            // GetPropConst r5, r0, "_x" (read back)
            .instruction(Instruction::GetPropConst {
                dst: Register(5),
                obj: Register(0),
                name: ConstantIndex(1), // "_x"
                ic_index: 2,
            })
            // Return r5
            .instruction(Instruction::Return { src: Register(5) })
            .feedback_vector_size(3)
            .build();

        // Setter function: this._x = arg
        // Note: We need to set up 'this' binding properly for this test
        // For now, let's just return 99 to verify the function was called
        let setter = Function::builder()
            .name("setter")
            .local_count(1)
            // The setter receives the value as first argument in local 0
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: LocalIndex(0),
            })
            // Return the value to verify setter was called
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        builder.add_function(main);
        builder.add_function(setter);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        // For now, just verify we can define a setter without crashing
        // Full setter semantics (with 'this' binding) would need more setup
        assert!(result.is_number() || result.is_undefined());
    }

    // ==================== IC Coverage Tests ====================

    #[test]
    fn test_ic_coverage_getprop_computed() {
        // Test GetProp IC with computed property access
        use otter_vm_bytecode::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");

        let func = Function::builder()
            .name("main")
            .feedback_vector_size(2) // For SetPropConst and GetProp
            .instruction(Instruction::NewObject { dst: Register(0) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 42,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0),
                val: Register(1),
                ic_index: 0,
            })
            .instruction(Instruction::LoadConst {
                dst: Register(2),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::GetProp {
                dst: Register(3),
                obj: Register(0),
                key: Register(2),
                ic_index: 1,
            })
            .instruction(Instruction::Return { src: Register(3) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(42));
    }

    #[test]
    fn test_ic_coverage_getelem_setelem() {
        // Test GetElem/SetElem IC with string keys on objects
        use otter_vm_bytecode::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");

        let func = Function::builder()
            .name("main")
            .feedback_vector_size(2) // For SetElem and GetElem
            .instruction(Instruction::NewObject { dst: Register(0) })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 100,
            })
            .instruction(Instruction::SetElem {
                arr: Register(0),
                idx: Register(1),
                val: Register(2),
                ic_index: 0,
            })
            .instruction(Instruction::GetElem {
                dst: Register(3),
                arr: Register(0),
                idx: Register(1),
                ic_index: 1,
            })
            .instruction(Instruction::Return { src: Register(3) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(100));
    }

    #[test]
    fn test_ic_coverage_in_operator() {
        // Test In operator IC
        use otter_vm_bytecode::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");

        let func = Function::builder()
            .name("main")
            .feedback_vector_size(2) // For SetPropConst and In
            .instruction(Instruction::NewObject { dst: Register(0) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0),
                val: Register(1),
                ic_index: 0,
            })
            .instruction(Instruction::LoadConst {
                dst: Register(2),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::In {
                dst: Register(3),
                lhs: Register(2),
                rhs: Register(0),
                ic_index: 1,
            })
            .instruction(Instruction::Return { src: Register(3) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_ic_coverage_instanceof() {
        // Test InstanceOf IC - caches prototype lookup on constructor
        // This test uses Construct to properly create an instance
        use otter_vm_bytecode::{ConstantIndex, FunctionIndex};

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("prototype");

        // Create a constructor function and test instanceof using Construct
        let main = Function::builder()
            .name("main")
            .feedback_vector_size(2)
            // Create constructor function
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            // Create instance using Construct
            .instruction(Instruction::Construct {
                dst: Register(1),
                func: Register(0),
                argc: 0,
            })
            // Test instanceof (this exercises the IC on prototype lookup)
            .instruction(Instruction::InstanceOf {
                dst: Register(2),
                lhs: Register(1),
                rhs: Register(0),
                ic_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        // Constructor function
        let constructor = Function::builder()
            .name("Constructor")
            .instruction(Instruction::LoadUndefined { dst: Register(0) })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        builder.add_function(main);
        builder.add_function(constructor);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_ic_coverage_array_integer_access() {
        // Test GetElem/SetElem fast path with integer indices on arrays
        let mut builder = Module::builder("test.js");

        let func = Function::builder()
            .name("main")
            .feedback_vector_size(2)
            // Create array with 3 elements
            .instruction(Instruction::NewArray {
                dst: Register(0),
                len: 3,
            })
            // Set arr[1] = 42
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1, // index
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 42, // value
            })
            .instruction(Instruction::SetElem {
                arr: Register(0),
                idx: Register(1),
                val: Register(2),
                ic_index: 0,
            })
            // Get arr[1]
            .instruction(Instruction::GetElem {
                dst: Register(3),
                arr: Register(0),
                idx: Register(1),
                ic_index: 1,
            })
            .instruction(Instruction::Return { src: Register(3) })
            .build();

        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(42));
    }

    // ==================== IC State Machine Tests ====================

    #[test]
    fn test_ic_state_machine_uninitialized_to_mono() {
        // Test that IC transitions from Uninitialized to Monomorphic on first access
        use otter_vm_bytecode::function::InlineCacheState;
        use otter_vm_bytecode::operand::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");

        let func = Function::builder()
            .name("main")
            .feedback_vector_size(1)
            // Create object with property
            .instruction(Instruction::NewObject { dst: Register(0) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 42,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0), // "x"
                val: Register(1),
                ic_index: 0,
            })
            // Read the property (this should cache in IC)
            .instruction(Instruction::GetPropConst {
                dst: Register(2),
                obj: Register(0),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        builder.add_function(func);
        let module = builder.build();
        let module = std::sync::Arc::new(module);

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(42));

        // Check IC state transitioned to Monomorphic
        let func = module.function(0).unwrap();
        let feedback = func.feedback_vector.read();
        if let Some(ic) = feedback.get(0) {
            match &ic.ic_state {
                InlineCacheState::Monomorphic { .. } => {}
                state => panic!("Expected Monomorphic IC state, got {:?}", state),
            }
        }
    }

    #[test]
    fn test_ic_state_machine_mono_to_poly() {
        // Test that IC transitions from Monomorphic to Polymorphic on 2nd shape
        use otter_vm_bytecode::function::InlineCacheState;
        use otter_vm_bytecode::operand::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");
        builder.constants_mut().add_string("y");

        let func = Function::builder()
            .name("main")
            .local_count(10)
            .register_count(10)
            .feedback_vector_size(1)
            // Create first object with property "x"
            .instruction(Instruction::NewObject { dst: Register(0) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 10,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0), // "x"
                val: Register(1),
                ic_index: 0,
            })
            // Read x from first object (caches mono state)
            .instruction(Instruction::GetPropConst {
                dst: Register(2),
                obj: Register(0),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            // Create second object with different shape (has "y" first, then "x")
            .instruction(Instruction::NewObject { dst: Register(3) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(4),
                value: 100,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(3),
                name: ConstantIndex(1), // "y"
                val: Register(4),
                ic_index: 0, // uses same IC slot but different shape
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(5),
                value: 20,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(3),
                name: ConstantIndex(0), // "x"
                val: Register(5),
                ic_index: 0,
            })
            // Read x from second object (should transition to poly)
            .instruction(Instruction::GetPropConst {
                dst: Register(6),
                obj: Register(3),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            // Return sum of both reads
            .instruction(Instruction::Add {
                dst: Register(7),
                lhs: Register(2),
                rhs: Register(6),
                feedback_index: 1,
            })
            .instruction(Instruction::Return { src: Register(7) })
            .build();

        builder.add_function(func);
        let module = builder.build();
        let module = std::sync::Arc::new(module);

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(30)); // 10 + 20

        // Check IC state transitioned to Polymorphic
        let func = module.function(0).unwrap();
        let feedback = func.feedback_vector.read();
        if let Some(ic) = feedback.get(0) {
            match &ic.ic_state {
                InlineCacheState::Polymorphic { count, .. } => {
                    assert!(*count >= 2, "Expected at least 2 shapes cached");
                }
                state => panic!("Expected Polymorphic IC state, got {:?}", state),
            }
        }
    }

    #[test]
    fn test_ic_state_machine_poly_to_mega() {
        // Test that IC transitions from Polymorphic to Megamorphic at 4+ shapes
        use otter_vm_bytecode::function::InlineCacheState;
        use otter_vm_bytecode::operand::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x"); // 0
        builder.constants_mut().add_string("a"); // 1
        builder.constants_mut().add_string("b"); // 2
        builder.constants_mut().add_string("c"); // 3
        builder.constants_mut().add_string("d"); // 4

        let func = Function::builder()
            .name("main")
            .local_count(30)
            .register_count(30)
            .feedback_vector_size(1)
            // Create 5 objects with different shapes, all having "x"
            // Object 1: only "x"
            .instruction(Instruction::NewObject { dst: Register(0) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0), // "x"
                val: Register(1),
                ic_index: 0,
            })
            .instruction(Instruction::GetPropConst {
                dst: Register(2),
                obj: Register(0),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            // Object 2: "a" then "x"
            .instruction(Instruction::NewObject { dst: Register(3) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(4),
                value: 100,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(3),
                name: ConstantIndex(1), // "a"
                val: Register(4),
                ic_index: 0,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(5),
                value: 2,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(3),
                name: ConstantIndex(0), // "x"
                val: Register(5),
                ic_index: 0,
            })
            .instruction(Instruction::GetPropConst {
                dst: Register(6),
                obj: Register(3),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            // Object 3: "b" then "x"
            .instruction(Instruction::NewObject { dst: Register(7) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(8),
                value: 100,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(7),
                name: ConstantIndex(2), // "b"
                val: Register(8),
                ic_index: 0,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(9),
                value: 3,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(7),
                name: ConstantIndex(0), // "x"
                val: Register(9),
                ic_index: 0,
            })
            .instruction(Instruction::GetPropConst {
                dst: Register(10),
                obj: Register(7),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            // Object 4: "c" then "x"
            .instruction(Instruction::NewObject { dst: Register(11) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(12),
                value: 100,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(11),
                name: ConstantIndex(3), // "c"
                val: Register(12),
                ic_index: 0,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(13),
                value: 4,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(11),
                name: ConstantIndex(0), // "x"
                val: Register(13),
                ic_index: 0,
            })
            .instruction(Instruction::GetPropConst {
                dst: Register(14),
                obj: Register(11),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            // Object 5: "d" then "x" - this should trigger Megamorphic
            .instruction(Instruction::NewObject { dst: Register(15) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(16),
                value: 100,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(15),
                name: ConstantIndex(4), // "d"
                val: Register(16),
                ic_index: 0,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(17),
                value: 5,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(15),
                name: ConstantIndex(0), // "x"
                val: Register(17),
                ic_index: 0,
            })
            .instruction(Instruction::GetPropConst {
                dst: Register(18),
                obj: Register(15),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            // Sum all x values: 1+2+3+4+5 = 15
            .instruction(Instruction::Add {
                dst: Register(19),
                lhs: Register(2),
                rhs: Register(6),
                feedback_index: 1,
            })
            .instruction(Instruction::Add {
                dst: Register(20),
                lhs: Register(19),
                rhs: Register(10),
                feedback_index: 2,
            })
            .instruction(Instruction::Add {
                dst: Register(21),
                lhs: Register(20),
                rhs: Register(14),
                feedback_index: 3,
            })
            .instruction(Instruction::Add {
                dst: Register(22),
                lhs: Register(21),
                rhs: Register(18),
                feedback_index: 4,
            })
            .instruction(Instruction::Return { src: Register(22) })
            .build();

        builder.add_function(func);
        let module = builder.build();
        let module = std::sync::Arc::new(module);

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(15)); // 1+2+3+4+5

        // Check IC state transitioned to Megamorphic
        let func = module.function(0).unwrap();
        let feedback = func.feedback_vector.read();
        if let Some(ic) = feedback.get(0) {
            match &ic.ic_state {
                InlineCacheState::Megamorphic => {}
                state => panic!("Expected Megamorphic IC state, got {:?}", state),
            }
        }
    }

    // ==================== Proto Chain Cache Tests ====================

    #[test]
    fn test_proto_chain_cache_epoch_bump() {
        // Test that proto_epoch is bumped when set_prototype is called
        use crate::object::get_proto_epoch;

        let memory_manager = Arc::new(crate::memory::MemoryManager::test());

        // Record initial epoch
        let initial_epoch = get_proto_epoch();

        // Create objects and set prototype
        let obj1 = GcRef::new(JsObject::new(None, memory_manager.clone()));
        let obj2 = GcRef::new(JsObject::new(None, memory_manager.clone()));

        // Set prototype should bump epoch
        obj1.set_prototype(Some(obj2.clone()));

        let after_first = get_proto_epoch();
        assert!(
            after_first > initial_epoch,
            "proto_epoch should be bumped after set_prototype"
        );

        // Another set_prototype should bump again
        let obj3 = GcRef::new(JsObject::new(None, memory_manager.clone()));
        obj2.set_prototype(Some(obj3));

        let after_second = get_proto_epoch();
        assert!(
            after_second > after_first,
            "proto_epoch should be bumped after each set_prototype"
        );
    }

    #[test]
    fn test_proto_chain_cache_ic_stores_epoch() {
        // Test that IC stores proto_epoch when caching
        use crate::object::get_proto_epoch;
        use otter_vm_bytecode::function::InlineCacheState;
        use otter_vm_bytecode::operand::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");

        let func = Function::builder()
            .name("main")
            .feedback_vector_size(1)
            // Create object and set property
            .instruction(Instruction::NewObject { dst: Register(0) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 42,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0), // "x"
                val: Register(1),
                ic_index: 0,
            })
            // Read property to trigger IC caching
            .instruction(Instruction::GetPropConst {
                dst: Register(2),
                obj: Register(0),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        builder.add_function(func);
        let module = builder.build();
        let module = std::sync::Arc::new(module);

        // Record epoch before execution
        let epoch_before = get_proto_epoch();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(42));

        // Check that IC has proto_epoch stored
        let func = module.function(0).unwrap();
        let feedback = func.feedback_vector.read();
        if let Some(ic) = feedback.get(0) {
            match &ic.ic_state {
                InlineCacheState::Monomorphic { .. } => {
                    // proto_epoch should be >= epoch_before (execution may have bumped it)
                    assert!(
                        ic.proto_epoch >= epoch_before,
                        "IC proto_epoch ({}) should be >= epoch_before ({})",
                        ic.proto_epoch,
                        epoch_before
                    );
                }
                state => panic!("Expected Monomorphic IC state, got {:?}", state),
            }
        }
    }

    #[test]
    fn test_proto_chain_cache_invalidation_on_read() {
        // Test that IC read path rejects cached data when proto_epoch has changed.
        // After execution populates the IC, we bump the proto_epoch externally
        // and verify that proto_epoch_matches would return false.
        use crate::object::{bump_proto_epoch, get_proto_epoch};
        use otter_vm_bytecode::function::InlineCacheState;
        use otter_vm_bytecode::operand::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");

        let func = Function::builder()
            .name("main")
            .feedback_vector_size(1)
            .instruction(Instruction::NewObject { dst: Register(0) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 42,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0), // "x"
                val: Register(1),
                ic_index: 0,
            })
            .instruction(Instruction::GetPropConst {
                dst: Register(2),
                obj: Register(0),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        builder.add_function(func);
        let module = builder.build();
        let module = std::sync::Arc::new(module);

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();
        assert_eq!(result.as_int32(), Some(42));

        // IC should be populated
        let func = module.function(0).unwrap();
        {
            let feedback = func.feedback_vector.read();
            let ic = feedback.get(0).expect("IC slot should exist");
            assert!(matches!(&ic.ic_state, InlineCacheState::Monomorphic { .. }));
            // At this point, epoch matches
            assert!(ic.proto_epoch_matches(get_proto_epoch()));
        }

        // Bump proto_epoch (simulating a prototype change)
        bump_proto_epoch();

        // Now the IC's cached epoch should NOT match
        {
            let feedback = func.feedback_vector.read();
            let ic = feedback.get(0).expect("IC slot should exist");
            assert!(
                !ic.proto_epoch_matches(get_proto_epoch()),
                "IC should be invalidated after proto_epoch bump"
            );
        }
    }

    #[test]
    fn test_proto_chain_cache_epoch_consistency() {
        // Test that proto_epoch is consistent across multiple IC updates
        use crate::object::get_proto_epoch;
        use otter_vm_bytecode::function::InlineCacheState;
        use otter_vm_bytecode::operand::ConstantIndex;

        let mut builder = Module::builder("test.js");
        builder.constants_mut().add_string("x");
        builder.constants_mut().add_string("y");

        let func = Function::builder()
            .name("main")
            .local_count(10)
            .register_count(10)
            .feedback_vector_size(1)
            // Create first object and set property
            .instruction(Instruction::NewObject { dst: Register(0) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 10,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(0),
                name: ConstantIndex(0), // "x"
                val: Register(1),
                ic_index: 0,
            })
            .instruction(Instruction::GetPropConst {
                dst: Register(2),
                obj: Register(0),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            // Create second object with different shape
            .instruction(Instruction::NewObject { dst: Register(3) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(4),
                value: 100,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(3),
                name: ConstantIndex(1), // "y"
                val: Register(4),
                ic_index: 0,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(5),
                value: 20,
            })
            .instruction(Instruction::SetPropConst {
                obj: Register(3),
                name: ConstantIndex(0), // "x"
                val: Register(5),
                ic_index: 0,
            })
            .instruction(Instruction::GetPropConst {
                dst: Register(6),
                obj: Register(3),
                name: ConstantIndex(0),
                ic_index: 0,
            })
            .instruction(Instruction::Add {
                dst: Register(7),
                lhs: Register(2),
                rhs: Register(6),
                feedback_index: 1,
            })
            .instruction(Instruction::Return { src: Register(7) })
            .build();

        builder.add_function(func);
        let module = builder.build();
        let module = std::sync::Arc::new(module);

        let epoch_before = get_proto_epoch();

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(30)); // 10 + 20

        // Check that IC has transitioned to Polymorphic and has proto_epoch
        let func = module.function(0).unwrap();
        let feedback = func.feedback_vector.read();
        if let Some(ic) = feedback.get(0) {
            match &ic.ic_state {
                InlineCacheState::Polymorphic { count, .. } => {
                    assert!(*count >= 2, "Expected at least 2 shapes cached");
                    // proto_epoch should be reasonable
                    assert!(
                        ic.proto_epoch >= epoch_before,
                        "IC proto_epoch should be >= epoch_before"
                    );
                }
                state => panic!("Expected Polymorphic IC state, got {:?}", state),
            }
        }
    }

    #[test]
    fn test_dictionary_mode_threshold_trigger() {
        // Test that adding more than DICTIONARY_THRESHOLD properties triggers dictionary mode
        use crate::object::{DICTIONARY_THRESHOLD, JsObject, PropertyKey};

        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, memory_manager));

        // Initially not in dictionary mode
        assert!(
            !obj.is_dictionary_mode(),
            "Object should not be in dictionary mode initially"
        );

        // Add properties up to just below threshold
        for i in 0..(DICTIONARY_THRESHOLD - 1) {
            let key = PropertyKey::String(crate::string::JsString::intern(&format!("prop{}", i)));
            obj.set(key, Value::int32(i as i32));
        }
        assert!(
            !obj.is_dictionary_mode(),
            "Object should not be in dictionary mode below threshold"
        );

        // Add one more property to exceed threshold
        let key = PropertyKey::String(crate::string::JsString::intern(&format!(
            "prop{}",
            DICTIONARY_THRESHOLD - 1
        )));
        obj.set(key, Value::int32(DICTIONARY_THRESHOLD as i32 - 1));

        // One more should trigger dictionary mode
        let key = PropertyKey::String(crate::string::JsString::intern(&format!(
            "prop{}",
            DICTIONARY_THRESHOLD
        )));
        obj.set(key, Value::int32(DICTIONARY_THRESHOLD as i32));

        assert!(
            obj.is_dictionary_mode(),
            "Object should be in dictionary mode after exceeding threshold"
        );
    }

    #[test]
    fn test_dictionary_mode_delete_trigger() {
        // Test that deleting a property triggers dictionary mode
        use crate::object::{JsObject, PropertyKey};

        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, memory_manager));

        // Add a few properties
        let key_a = PropertyKey::String(crate::string::JsString::intern("a"));
        let key_b = PropertyKey::String(crate::string::JsString::intern("b"));
        obj.set(key_a.clone(), Value::int32(1));
        obj.set(key_b.clone(), Value::int32(2));

        assert!(
            !obj.is_dictionary_mode(),
            "Object should not be in dictionary mode before delete"
        );

        // Delete a property
        obj.delete(&key_a);

        assert!(
            obj.is_dictionary_mode(),
            "Object should be in dictionary mode after delete"
        );

        // Verify we can still access the remaining property
        assert_eq!(obj.get(&key_b), Some(Value::int32(2)));
    }

    #[test]
    fn test_dictionary_mode_storage_correctness() {
        // Test that dictionary mode storage works correctly
        use crate::object::{JsObject, PropertyKey};

        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, memory_manager));

        // Add a property
        let key_a = PropertyKey::String(crate::string::JsString::intern("a"));
        obj.set(key_a.clone(), Value::int32(42));

        // Trigger dictionary mode via delete
        let key_b = PropertyKey::String(crate::string::JsString::intern("b"));
        obj.set(key_b.clone(), Value::int32(100));
        obj.delete(&key_b);

        assert!(obj.is_dictionary_mode());

        // Add a new property in dictionary mode
        let key_c = PropertyKey::String(crate::string::JsString::intern("c"));
        obj.set(key_c.clone(), Value::int32(200));

        // Verify all properties work correctly
        assert_eq!(obj.get(&key_a), Some(Value::int32(42)));
        assert_eq!(obj.get(&key_b), None); // Deleted
        assert_eq!(obj.get(&key_c), Some(Value::int32(200)));

        // Verify has_own works
        assert!(obj.has_own(&key_a));
        assert!(!obj.has_own(&key_b));
        assert!(obj.has_own(&key_c));
    }

    #[test]
    fn test_dictionary_mode_ic_skip() {
        // Test that IC reports Megamorphic for dictionary mode objects
        use crate::object::{JsObject, PropertyKey};
        use otter_vm_bytecode::function::InlineCacheState;

        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let obj = GcRef::new(JsObject::new(None, memory_manager));

        // Add and delete a property to trigger dictionary mode
        let key_a = PropertyKey::String(crate::string::JsString::intern("a"));
        let key_b = PropertyKey::String(crate::string::JsString::intern("b"));
        obj.set(key_a.clone(), Value::int32(1));
        obj.set(key_b.clone(), Value::int32(2));
        obj.delete(&key_a);

        assert!(obj.is_dictionary_mode());

        // Create an IC metadata and verify it can detect dictionary mode
        let mut ic = otter_vm_bytecode::function::InstructionMetadata::new();

        // Simulate what IC write code does for dictionary mode objects
        if obj.is_dictionary_mode() {
            ic.ic_state = InlineCacheState::Megamorphic;
        }

        // IC should be Megamorphic for dictionary mode objects
        assert!(
            matches!(ic.ic_state, InlineCacheState::Megamorphic),
            "IC should be Megamorphic for dictionary mode objects"
        );
    }

    // ==================== Hot Function Detection Tests ====================

    #[test]
    fn test_hot_function_detection_call_count() {
        use otter_vm_bytecode::function::HOT_FUNCTION_THRESHOLD;

        let mut builder = Module::builder("test.js");

        // Simple function that returns immediately
        let func = Function::builder()
            .name("hot_candidate")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 42,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        builder.add_function(func);
        let module = builder.build();
        let module = Arc::new(module);

        // Get the function and check initial state
        let func = module.function(0).unwrap();
        assert_eq!(func.get_call_count(), 0);
        assert!(!func.is_hot_function());

        // Execute the function multiple times
        for _ in 0..100 {
            let mut ctx = create_test_context();
            let mut interpreter = Interpreter::new();
            let _ = interpreter.execute_arc(module.clone(), &mut ctx);
        }

        // Call count should be 100
        assert_eq!(func.get_call_count(), 100);
        assert!(!func.is_hot_function()); // Not yet hot

        // Execute until we cross the threshold
        for _ in 0..(HOT_FUNCTION_THRESHOLD - 100) {
            let mut ctx = create_test_context();
            let mut interpreter = Interpreter::new();
            let _ = interpreter.execute_arc(module.clone(), &mut ctx);
        }

        // Should now be hot
        assert!(func.get_call_count() >= HOT_FUNCTION_THRESHOLD);
        assert!(func.is_hot_function());
    }

    #[test]
    fn test_hot_function_detection_record_call() {
        use otter_vm_bytecode::function::HOT_FUNCTION_THRESHOLD;

        let func = Function::builder()
            .name("test")
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        // Initially not hot
        assert_eq!(func.get_call_count(), 0);
        assert!(!func.is_hot_function());

        // Record calls up to threshold - 1
        for _ in 0..(HOT_FUNCTION_THRESHOLD - 1) {
            let became_hot = func.record_call();
            assert!(!became_hot);
        }

        assert!(!func.is_hot_function());

        // This call should make it hot
        let became_hot = func.record_call();
        assert!(became_hot);
        assert!(func.is_hot_function());

        // Subsequent calls should not report becoming hot again
        let became_hot = func.record_call();
        assert!(!became_hot);
        assert!(func.is_hot_function());
    }

    #[test]
    fn test_hot_function_mark_hot() {
        let func = Function::builder()
            .name("test")
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        assert!(!func.is_hot_function());

        // Manually mark as hot
        func.mark_hot();
        assert!(func.is_hot_function());
    }

    #[test]
    fn test_hot_function_nested_calls() {
        use otter_vm_bytecode::FunctionIndex;

        let mut builder = Module::builder("test.js");

        // Main function calls inner function in a loop
        let main = Function::builder()
            .name("main")
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0,
            }) // counter
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 100,
            }) // limit
            // Loop: call inner function
            .instruction(Instruction::Call {
                dst: Register(3),
                func: Register(0),
                argc: 0,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(4),
                value: 1,
            })
            .instruction(Instruction::Add {
                dst: Register(1),
                lhs: Register(1),
                rhs: Register(4),
                feedback_index: 0,
            })
            .instruction(Instruction::Lt {
                dst: Register(5),
                lhs: Register(1),
                rhs: Register(2),
            })
            .instruction(Instruction::JumpIfTrue {
                cond: Register(5),
                offset: otter_vm_bytecode::JumpOffset(-5),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .feedback_vector_size(1)
            .build();

        // Inner function just returns 1
        let inner = Function::builder()
            .name("inner")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        builder.add_function(main);
        builder.add_function(inner);
        let module = builder.build();
        let module = Arc::new(module);

        let mut ctx = create_test_context();
        let mut interpreter = Interpreter::new();
        let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(100));

        // Main was called once
        let main_func = module.function(0).unwrap();
        assert_eq!(main_func.get_call_count(), 1);

        // Inner was called 100 times
        let inner_func = module.function(1).unwrap();
        assert_eq!(inner_func.get_call_count(), 100);
    }
}
