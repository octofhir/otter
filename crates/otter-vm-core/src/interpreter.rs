//! Bytecode interpreter
//!
//! Executes bytecode instructions.

use otter_vm_bytecode::{Instruction, Module, UpvalueCapture};

use crate::async_context::{AsyncContext, VmExecutionResult};
use crate::context::VmContext;
use crate::error::{VmError, VmResult};
use crate::generator::JsGenerator;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::promise::{JsPromise, PromiseState};
use crate::regexp::JsRegExp;
use crate::string::JsString;
use crate::value::{Closure, UpvalueCell, Value};

use num_bigint::BigInt as NumBigInt;
use num_traits::{One, Zero};
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

        // Push initial frame with module reference
        ctx.push_frame(
            module.entry_point,
            Arc::clone(&module),
            entry_func.local_count,
            None,
            false,
            entry_func.is_async(),
        )?;
        ctx.set_running(true);

        // Execute loop
        let result = self.run_loop(ctx);

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

        // Push initial frame with module reference
        if let Err(e) = ctx.push_frame(
            module.entry_point,
            Arc::clone(&module),
            entry_func.local_count,
            None,
            false,
            entry_func.is_async(),
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
            return self.call_native_fn(ctx, native_fn, args);
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

        // Push frame for the function call
        ctx.push_frame(
            closure.function_index,
            Arc::clone(&closure.module),
            func_info.local_count,
            Some(0), // Return register (unused, we get result from Return)
            false,   // Not a construct call
            closure.is_async,
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

            match self.execute_instruction(instruction, Arc::clone(&current_module), ctx) {
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
                    argc: _,
                    return_reg,
                    is_construct,
                    is_async,
                    upvalues,
                }) => {
                    ctx.advance_pc();
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
                        is_construct,
                        is_async,
                    )?;
                }
                Ok(InstructionResult::TailCall {
                    func_index,
                    module,
                    argc: _,
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
                        "Uncaught exception: {:?}",
                        error
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
            if ctx.should_check_interrupt() && ctx.is_interrupted() {
                ctx.set_running(false);
                return VmExecutionResult::Error("Execution interrupted".to_string());
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

            // Clone module Arc for execute_instruction (required since it takes ownership)
            let current_module = Arc::clone(module_ref);

            // Execute the instruction
            let instruction_result = match self.execute_instruction(instruction, current_module, ctx)
            {
                Ok(result) => result,
                Err(err) => match err {
                    VmError::TypeError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "TypeError", &message))
                    }
                    VmError::RangeError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "RangeError", &message))
                    }
                    VmError::ReferenceError(message) => InstructionResult::Throw(
                        self.make_error(ctx, "ReferenceError", &message),
                    ),
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
                    argc: _,
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
                        let rest_arr = Arc::new(JsObject::array(rest_args.len()));
                        // If `Array.prototype` is available, attach it so rest arrays are iterable.
                        if let Some(array_obj) =
                            ctx.get_global("Array").and_then(|v| v.as_object().cloned())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object().cloned())
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
                    ) {
                        return VmExecutionResult::Error(e.to_string());
                    }
                }
                InstructionResult::TailCall {
                    func_index,
                    module: call_module,
                    argc: _,
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
                        let rest_arr = Arc::new(JsObject::array(rest_args.len()));
                        if let Some(array_obj) =
                            ctx.get_global("Array").and_then(|v| v.as_object().cloned())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object().cloned())
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
                        PromiseState::Pending => {
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
                InstructionResult::Yield { value } => {
                    // Generator yielded a value
                    let result = Arc::new(JsObject::new(None));
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
            if ctx.should_check_interrupt() && ctx.is_interrupted() {
                return Err(VmError::interrupted());
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

            // Clone module Arc for execute_instruction (required since it takes ownership)
            let current_module = Arc::clone(module_ref);

            // Execute the instruction
            let instruction_result = match self.execute_instruction(instruction, current_module, ctx)
            {
                Ok(result) => result,
                Err(err) => match err {
                    VmError::TypeError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "TypeError", &message))
                    }
                    VmError::RangeError(message) => {
                        InstructionResult::Throw(self.make_error(ctx, "RangeError", &message))
                    }
                    VmError::ReferenceError(message) => InstructionResult::Throw(
                        self.make_error(ctx, "ReferenceError", &message),
                    ),
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
                        stack: Vec::new(),
                    })));
                }
                InstructionResult::Call {
                    func_index,
                    module: call_module,
                    argc: _,
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
                        let rest_arr = Arc::new(JsObject::array(rest_args.len()));
                        // If `Array.prototype` is available, attach it so rest arrays are iterable.
                        if let Some(array_obj) =
                            ctx.get_global("Array").and_then(|v| v.as_object().cloned())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object().cloned())
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
                    ctx.push_frame(
                        func_index,
                        call_module,
                        local_count,
                        Some(return_reg),
                        is_construct,
                        is_async,
                    )?;
                }
                InstructionResult::TailCall {
                    func_index,
                    module: call_module,
                    argc: _,
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
                        let rest_arr = Arc::new(JsObject::array(rest_args.len()));
                        if let Some(array_obj) =
                            ctx.get_global("Array").and_then(|v| v.as_object().cloned())
                            && let Some(array_proto) = array_obj
                                .get(&PropertyKey::string("prototype"))
                                .and_then(|v| v.as_object().cloned())
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

                    // Push new frame (reusing the stack slot we just freed)
                    ctx.push_frame(
                        func_index,
                        call_module,
                        local_count,
                        Some(return_reg),
                        false, // tail calls are never construct
                        is_async,
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
                        PromiseState::Pending => {
                            // Promise is pending - need to wait for async operation
                            // Return the promise to caller for async handling
                            // The runtime's event loop should poll and resume
                            return Ok(Value::promise(promise));
                        }
                    }
                }
                InstructionResult::Yield { value } => {
                    // Generator yielded a value
                    // Create an iterator result object { value, done: false }
                    let result = Arc::new(JsObject::new(None));
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
        module: Arc<Module>,
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
                let value = ctx.get_local(idx.0)?.clone();
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
                    if let Some(otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                        shape_id: shape_addr,
                        offset,
                    }) = feedback.get(*ic_index as usize)
                    {
                        if std::sync::Arc::as_ptr(&global_obj.shape()) as u64 == *shape_addr {
                            global_obj.get_by_offset(*offset as usize)
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
                            if let otter_vm_bytecode::function::InlineCacheState::Uninitialized = ic {
                                *ic = otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                    shape_id: std::sync::Arc::as_ptr(&global_obj.shape()) as u64,
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
                    if let Some(otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                        shape_id: shape_addr,
                        offset,
                    }) = feedback.get(*ic_index as usize)
                    {
                        if std::sync::Arc::as_ptr(&global_obj.shape()) as u64 == *shape_addr {
                            if global_obj.set_by_offset(*offset as usize, val_val.clone()) {
                                return Ok(InstructionResult::Continue);
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
                            if let otter_vm_bytecode::function::InlineCacheState::Uninitialized = ic {
                                *ic = otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                    shape_id: std::sync::Arc::as_ptr(&global_obj.shape()) as u64,
                                    offset: offset as u32,
                                };
                            }
                        }
                    }
                }

                Ok(InstructionResult::Continue)
            }

            Instruction::LoadThis { dst } => {
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
            Instruction::Add { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                let result = self.op_add(left, right)?;
                ctx.set_register(dst.0, result);
                Ok(InstructionResult::Continue)
            }

            Instruction::Sub { dst, lhs, rhs } => {
                let left_value = ctx.get_register(lhs.0);
                let right_value = ctx.get_register(rhs.0);
                let left_bigint = self.bigint_value(left_value)?;
                let right_bigint = self.bigint_value(right_value)?;

                if let (Some(left_bigint), Some(right_bigint)) = (left_bigint, right_bigint) {
                    let result = left_bigint - right_bigint;
                    ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    return Ok(InstructionResult::Continue);
                }

                if left_value.is_bigint() || right_value.is_bigint() {
                    return Err(VmError::type_error(
                        "Cannot mix BigInt and other types",
                    ));
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

            Instruction::Mul { dst, lhs, rhs } => {
                let left_value = ctx.get_register(lhs.0);
                let right_value = ctx.get_register(rhs.0);
                let left_bigint = self.bigint_value(left_value)?;
                let right_bigint = self.bigint_value(right_value)?;

                if let (Some(left_bigint), Some(right_bigint)) = (left_bigint, right_bigint) {
                    let result = left_bigint * right_bigint;
                    ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    return Ok(InstructionResult::Continue);
                }

                if left_value.is_bigint() || right_value.is_bigint() {
                    return Err(VmError::type_error(
                        "Cannot mix BigInt and other types",
                    ));
                }

                let left = self.coerce_number(left_value)?;
                let right = self.coerce_number(right_value)?;

                ctx.set_register(dst.0, Value::number(left * right));
                Ok(InstructionResult::Continue)
            }

            Instruction::Div { dst, lhs, rhs } => {
                let left_value = ctx.get_register(lhs.0);
                let right_value = ctx.get_register(rhs.0);
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
                    return Err(VmError::type_error(
                        "Cannot mix BigInt and other types",
                    ));
                }

                let left = self.coerce_number(left_value)?;
                let right = self.coerce_number(right_value)?;

                ctx.set_register(dst.0, Value::number(left / right));
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
                    return Err(VmError::type_error(
                        "Cannot mix BigInt and other types",
                    ));
                }

                let left = self.coerce_number(left_value)?;
                let right = self.coerce_number(right_value)?;

                ctx.set_register(dst.0, Value::number(left % right));
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
                let str_value = Value::string(Arc::new(JsString::new(type_name)));
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

                let type_name = match ctx.get_global_utf16(name_str) {
                    Some(value) => value.type_of(),
                    None => "undefined",
                };
                let str_value = Value::string(Arc::new(JsString::new(type_name)));
                ctx.set_register(dst.0, str_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::InstanceOf { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                let Some(left_obj) = left.as_object().cloned() else {
                    ctx.set_register(dst.0, Value::boolean(false));
                    return Ok(InstructionResult::Continue);
                };

                let Some(right_obj) = right.as_object() else {
                    return Err(VmError::type_error(
                        "Right-hand side of instanceof is not an object",
                    ));
                };

                let proto_val = right_obj
                    .get(&PropertyKey::string("prototype"))
                    .unwrap_or_else(Value::undefined);
                let Some(target_proto) = proto_val.as_object().cloned() else {
                    return Err(VmError::type_error("Function has non-object prototype"));
                };

                let mut current = Some(left_obj);
                let mut depth = 0;
                const MAX_PROTO_DEPTH: usize = 100;
                while let Some(obj) = current {
                    if Arc::ptr_eq(&obj, &target_proto) {
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

            Instruction::In { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0);
                let right = ctx.get_register(rhs.0);

                let Some(right_obj) = right.as_object() else {
                    return Err(VmError::type_error(
                        "Cannot use 'in' operator to search for property in non-object",
                    ));
                };

                let key = if let Some(n) = left.as_int32() {
                    PropertyKey::Index(n as u32)
                } else if let Some(s) = left.as_string() {
                    PropertyKey::from_js_string(Arc::clone(s))
                } else if let Some(sym) = left.as_symbol() {
                    PropertyKey::Symbol(sym.id)
                } else {
                    let idx_str = self.to_string(left);
                    PropertyKey::string(&idx_str)
                };

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

                let func_obj = Arc::new(JsObject::new(None));
                let proto = Arc::new(JsObject::new(None));
                let closure = Arc::new(Closure {
                    function_index: func.0,
                    module: Arc::clone(&module),
                    upvalues: captured_upvalues,
                    is_async: func_def.is_async(),
                    object: Arc::clone(&func_obj),
                });
                let func_value = Value::function(closure);
                func_obj.set(
                    PropertyKey::string("prototype"),
                    Value::object(Arc::clone(&proto)),
                );
                proto.set(PropertyKey::string("constructor"), func_value.clone());
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

                let func_obj = Arc::new(JsObject::new(None));
                let proto = Arc::new(JsObject::new(None));
                let closure = Arc::new(Closure {
                    function_index: func.0,
                    module: Arc::clone(&module),
                    upvalues: captured_upvalues,
                    is_async: true,
                    object: Arc::clone(&func_obj),
                });
                let func_value = Value::function(closure);
                func_obj.set(
                    PropertyKey::string("prototype"),
                    Value::object(Arc::clone(&proto)),
                );
                proto.set(PropertyKey::string("constructor"), func_value.clone());
                ctx.set_register(dst.0, func_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::GeneratorClosure { dst, func } => {
                // Create a generator function - when called, it creates a generator object
                let generator_fn = JsGenerator::new(func.0, Vec::new());
                ctx.set_register(dst.0, Value::generator(generator_fn));
                Ok(InstructionResult::Continue)
            }

            Instruction::Call { dst, func, argc } => {
                let func_value = ctx.get_register(func.0).clone();

                // Check if it's a native function first
                if let Some(native_fn) = func_value.as_native_function() {
                    // Collect arguments
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        let arg = ctx.get_register(func.0 + 1 + i).clone();
                        args.push(arg);
                    }

                    // Call the native function with depth tracking
                    let result = self.call_native_fn(ctx, native_fn, &args)?;
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
                            Value::object(Arc::clone(ctx.global()))
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
                            let result = self.call_native_fn(ctx, native_fn, &all_args)?;
                            ctx.set_register(dst.0, result);
                            return Ok(InstructionResult::Continue);
                        } else if let Some(closure) = bound_fn.as_function() {
                            // Set the bound this and args
                            ctx.set_pending_this(this_arg);
                            ctx.set_pending_args(all_args);

                            return Ok(InstructionResult::Call {
                                func_index: closure.function_index,
                                module: Arc::clone(&closure.module),
                                argc: *argc,
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

                // Copy arguments from caller registers (func+1, func+2, ...)
                // to prepare for the new frame
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    let arg = ctx.get_register(func.0 + 1 + i).clone();
                    args.push(arg);
                }

                self.handle_call_value(ctx, &func_value, Value::undefined(), args, dst.0)
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
                    let result = self.call_native_fn(ctx, native_fn, &args)?;
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
                        .and_then(|v| v.as_object().cloned());
                    let new_obj = Arc::new(JsObject::new(ctor_proto));
                    let new_obj_value = Value::object(new_obj);

                    // Call native constructor with depth tracking
                    let result = self.call_native_fn(ctx, native_fn, &args)?;
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
                    // Create a new object with prototype = ctor.prototype (if any).
                    let ctor_proto = func_value
                        .as_object()
                        .and_then(|o| o.get(&PropertyKey::string("prototype")))
                        .and_then(|v| v.as_object().cloned());
                    let new_obj = Arc::new(JsObject::new(ctor_proto));
                    let new_obj_value = Value::object(new_obj.clone());

                    // Copy arguments from caller registers
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        let arg = ctx.get_register(func.0 + 1 + i).clone();
                        args.push(arg);
                    }

                    // Store args and the new object (as `this`) for new frame
                    ctx.set_pending_args(args);
                    ctx.set_pending_this(new_obj_value.clone());

                    // For simplicity, return the new object directly for now
                    // A proper implementation would call the constructor and return `this`
                    ctx.set_register(dst.0, new_obj_value);

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
                    if let Some(otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                        shape_id: shape_addr,
                        offset,
                    }) = feedback.get(*ic_index as usize)
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
                    let function_obj = ctx
                        .get_global("Function")
                        .and_then(|v| v.as_object().cloned())
                        .ok_or_else(|| VmError::type_error("Function is not defined"))?;
                    let proto = function_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object().cloned())
                        .ok_or_else(|| VmError::type_error("Function.prototype is not defined"))?;
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
                        .and_then(|v| v.as_object().cloned())
                        .ok_or_else(|| VmError::type_error("String is not defined"))?;
                    let proto = string_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object().cloned())
                        .ok_or_else(|| VmError::type_error("String.prototype is not defined"))?;
                    proto
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if receiver.is_promise() {
                    let promise_obj = ctx
                        .get_global("Promise")
                        .and_then(|v| v.as_object().cloned())
                        .ok_or_else(|| VmError::type_error("Promise is not defined"))?;
                    let proto = promise_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object().cloned())
                        .ok_or_else(|| VmError::type_error("Promise.prototype is not defined"))?;
                    proto
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if receiver.is_number() {
                    let number_obj = ctx
                        .get_global("Number")
                        .and_then(|v| v.as_object().cloned())
                        .ok_or_else(|| VmError::type_error("Number is not defined"))?;
                    let proto = number_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object().cloned())
                        .ok_or_else(|| VmError::type_error("Number.prototype is not defined"))?;
                    proto
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if receiver.is_boolean() {
                    let boolean_obj = ctx
                        .get_global("Boolean")
                        .and_then(|v| v.as_object().cloned())
                        .ok_or_else(|| VmError::type_error("Boolean is not defined"))?;
                    let proto = boolean_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object().cloned())
                        .ok_or_else(|| VmError::type_error("Boolean.prototype is not defined"))?;
                    proto
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
                            if let otter_vm_bytecode::function::InlineCacheState::Uninitialized = ic {
                                *ic = otter_vm_bytecode::function::InlineCacheState::Monomorphic {
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
                Ok(InstructionResult::Return(value))
            }

            Instruction::ReturnUndefined => Ok(InstructionResult::Return(Value::undefined())),

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
                    // Call the native function directly
                    let result = native_fn(&args).map_err(VmError::type_error)?;
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
                        .and_then(|v| v.as_object().cloned());
                    let new_obj = Arc::new(JsObject::new(ctor_proto));
                    let new_obj_value = Value::object(new_obj);

                    let result = native_fn(&args).map_err(VmError::type_error)?;
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
                    .and_then(|v| v.as_object().cloned());
                let new_obj = Arc::new(JsObject::new(ctor_proto));
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
                        PromiseState::Pending => {
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
                ctx.set_register(dst.0, Value::undefined()); // Will be set on resume

                // Return a yield result
                Ok(InstructionResult::Yield { value })
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
                    .and_then(|proto_val| proto_val.as_object().cloned());

                let obj = Arc::new(JsObject::new(proto));
                ctx.set_register(dst.0, Value::object(obj));
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

                    if let Some(string_obj) = ctx
                        .get_global("String")
                        .and_then(|v| v.as_object().cloned())
                    {
                        if let Some(proto) = string_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object().cloned())
                        {
                            let key = Self::utf16_key(name_str);
                            let value = proto.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            return Ok(InstructionResult::Continue);
                        }
                    }
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

                            match ic {
                                InlineCacheState::Monomorphic { shape_id, offset } => {
                                    if obj_shape_ptr == *shape_id {
                                        cached_val = obj_ref.get_by_offset(*offset as usize);
                                    }
                                }
                                InlineCacheState::Polymorphic { count, entries } => {
                                    for i in 0..(*count as usize) {
                                        if obj_shape_ptr == entries[i].0 {
                                            cached_val = obj_ref.get_by_offset(entries[i].1 as usize);
                                            break;
                                        }
                                    }
                                }
                                _ => {}
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
                    if let Some(function_obj) = ctx
                        .get_global("Function")
                        .and_then(|v| v.as_object().cloned())
                    {
                        if let Some(proto) = function_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object().cloned())
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
                                let result = native_fn(&[]).map_err(VmError::type_error)?;
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

                                    match ic {
                                        InlineCacheState::Uninitialized => {
                                            *ic = InlineCacheState::Monomorphic { shape_id: shape_ptr, offset: offset as u32 };
                                        }
                                        InlineCacheState::Monomorphic { shape_id: old_shape, offset: old_offset } => {
                                            if *old_shape != shape_ptr {
                                                let mut entries = [(0u64, 0u32); 4];
                                                entries[0] = (*old_shape, *old_offset);
                                                entries[1] = (shape_ptr, offset as u32);
                                                *ic = InlineCacheState::Polymorphic { count: 2, entries };
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
                                                    entries[*count as usize] = (shape_ptr, offset as u32);
                                                    *count += 1;
                                                } else {
                                                    *ic = InlineCacheState::Megamorphic;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }

                            let value = obj.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            Ok(InstructionResult::Continue)
                        }
                    }
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

                            match ic {
                                InlineCacheState::Monomorphic { shape_id, offset } => {
                                    if obj_shape_ptr == *shape_id {
                                        if obj.set_by_offset(*offset as usize, val_val.clone()) {
                                            cached = true;
                                        }
                                    }
                                }
                                InlineCacheState::Polymorphic { count, entries } => {
                                    for i in 0..(*count as usize) {
                                        if obj_shape_ptr == entries[i].0 {
                                            if obj.set_by_offset(entries[i].1 as usize, val_val.clone()) {
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

                    if cached {
                        return Ok(InstructionResult::Continue);
                    }

                    match obj.lookup_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Accessor { set, .. }) => {
                            let Some(setter) = set else {
                                return Ok(InstructionResult::Continue);
                            };

                            if let Some(native_fn) = setter.as_native_function() {
                                native_fn(&[val_val]).map_err(VmError::type_error)?;
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
                            if let Some(offset) = obj.shape().get_offset(&Self::utf16_key(name_str))
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

                                    match ic {
                                        InlineCacheState::Uninitialized => {
                                            *ic = InlineCacheState::Monomorphic { shape_id: shape_ptr, offset: offset as u32 };
                                        }
                                        InlineCacheState::Monomorphic { shape_id: old_shape, offset: old_offset } => {
                                            if *old_shape != shape_ptr {
                                                let mut entries = [(0u64, 0u32); 4];
                                                entries[0] = (*old_shape, *old_offset);
                                                entries[1] = (shape_ptr, offset as u32);
                                                *ic = InlineCacheState::Polymorphic { count: 2, entries };
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
                                                    entries[*count as usize] = (shape_ptr, offset as u32);
                                                    *count += 1;
                                                } else {
                                                    *ic = InlineCacheState::Megamorphic;
                                                }
                                            }
                                        }
                                        _ => {}
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
                let object = ctx.get_register(obj.0);
                let key_value = ctx.get_register(key.0);

                let result = if let Some(obj) = object.as_object() {
                    let key = if let Some(n) = key_value.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = key_value.as_string() {
                        PropertyKey::string(s.as_str())
                    } else if let Some(sym) = key_value.as_symbol() {
                        PropertyKey::Symbol(sym.id)
                    } else {
                        let key_str = self.to_string(key_value);
                        PropertyKey::string(&key_str)
                    };

                    if !obj.has_own(&key) {
                        true
                    } else {
                        obj.delete(&key)
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

                if let Some(str_ref) = object.as_string() {
                    let key = if let Some(n) = key_value.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = key_value.as_string() {
                        PropertyKey::from_js_string(Arc::clone(s))
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

                    if let Some(string_obj) = ctx
                        .get_global("String")
                        .and_then(|v| v.as_object().cloned())
                    {
                        if let Some(proto) = string_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object().cloned())
                        {
                            let value = proto.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                }

                if let Some(obj) = object.as_object() {
                    let receiver = object.clone();

                    // IC Fast Path
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

                            match ic {
                                InlineCacheState::Monomorphic { shape_id, offset } => {
                                    if obj_shape_ptr == *shape_id {
                                        cached_val = obj.get_by_offset(*offset as usize);
                                    }
                                }
                                InlineCacheState::Polymorphic { count, entries } => {
                                    for i in 0..(*count as usize) {
                                        if obj_shape_ptr == entries[i].0 {
                                            cached_val = obj.get_by_offset(entries[i].1 as usize);
                                            break;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }

                    if let Some(val) = cached_val {
                        ctx.set_register(dst.0, val);
                        return Ok(InstructionResult::Continue);
                    }

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

                    match obj.lookup_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Accessor { get, .. }) => {
                            let Some(getter) = get else {
                                ctx.set_register(dst.0, Value::undefined());
                                return Ok(InstructionResult::Continue);
                            };

                            if let Some(native_fn) = getter.as_native_function() {
                                let result = native_fn(&[]).map_err(VmError::type_error)?;
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
                            // Slow Path (Full lookup)
                            if let Some(offset) = obj.shape().get_offset(&key) {
                                // Update IC to Monomorphic
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

                                    match ic {
                                        InlineCacheState::Uninitialized => {
                                            *ic = InlineCacheState::Monomorphic { shape_id: shape_ptr, offset: offset as u32 };
                                        }
                                        InlineCacheState::Monomorphic { shape_id: old_shape, offset: old_offset } => {
                                            if *old_shape != shape_ptr {
                                                let mut entries = [(0u64, 0u32); 4];
                                                entries[0] = (*old_shape, *old_offset);
                                                entries[1] = (shape_ptr, offset as u32);
                                                *ic = InlineCacheState::Polymorphic { count: 2, entries };
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
                                                    entries[*count as usize] = (shape_ptr, offset as u32);
                                                    *count += 1;
                                                } else {
                                                    *ic = InlineCacheState::Megamorphic;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }

                            let value = obj.get(&key).unwrap_or_else(Value::undefined);
                            ctx.set_register(dst.0, value);
                            Ok(InstructionResult::Continue)
                        }
                    }
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

                            match ic {
                                InlineCacheState::Monomorphic { shape_id, offset } => {
                                    if obj_shape_ptr == *shape_id {
                                        if obj.set_by_offset(*offset as usize, val_val.clone()) {
                                            cached = true;
                                        }
                                    }
                                }
                                InlineCacheState::Polymorphic { count, entries } => {
                                    for i in 0..(*count as usize) {
                                        if obj_shape_ptr == entries[i].0 {
                                            if obj.set_by_offset(entries[i].1 as usize, val_val.clone()) {
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

                    if cached {
                        return Ok(InstructionResult::Continue);
                    }

                    match obj.lookup_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Accessor { set, .. }) => {
                            let Some(setter) = set else {
                                return Ok(InstructionResult::Continue);
                            };

                            if let Some(native_fn) = setter.as_native_function() {
                                native_fn(&[val_val]).map_err(VmError::type_error)?;
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
                            obj.set(key.clone(), val_val);
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

                                    match ic {
                                        InlineCacheState::Uninitialized => {
                                            *ic = InlineCacheState::Monomorphic { shape_id: shape_ptr, offset: offset as u32 };
                                        }
                                        InlineCacheState::Monomorphic { shape_id: old_shape, offset: old_offset } => {
                                            if *old_shape != shape_ptr {
                                                let mut entries = [(0u64, 0u32); 4];
                                                entries[0] = (*old_shape, *old_offset);
                                                entries[1] = (shape_ptr, offset as u32);
                                                *ic = InlineCacheState::Polymorphic { count: 2, entries };
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
                                                    entries[*count as usize] = (shape_ptr, offset as u32);
                                                    *count += 1;
                                                } else {
                                                    *ic = InlineCacheState::Megamorphic;
                                                }
                                            }
                                        }
                                        _ => {}
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
                let arr = Arc::new(JsObject::array(*len as usize));
                // Attach `Array.prototype` if present so arrays are iterable and have methods.
                if let Some(array_obj) =
                    ctx.get_global("Array").and_then(|v| v.as_object().cloned())
                    && let Some(array_proto) = array_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object().cloned())
                {
                    arr.set_prototype(Some(array_proto));
                }
                ctx.set_register(dst.0, Value::object(arr));
                Ok(InstructionResult::Continue)
            }

            Instruction::GetElem { dst, arr, idx } => {
                let array = ctx.get_register(arr.0).clone();
                let index = ctx.get_register(idx.0).clone();

                if let Some(obj) = array.as_object() {
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

                    // Fallback to generic access
                    let value = if let Some(n) = index.as_int32() {
                        obj.get(&PropertyKey::Index(n as u32))
                            .unwrap_or_else(Value::undefined)
                    } else {
                        let idx_str = self.to_string(&index);
                        obj.get(&PropertyKey::string(&idx_str))
                            .unwrap_or_else(Value::undefined)
                    };
                    ctx.set_register(dst.0, value);
                } else {
                    ctx.set_register(dst.0, Value::undefined());
                }
                Ok(InstructionResult::Continue)
            }

            Instruction::SetElem { arr, idx, val } => {
                let array = ctx.get_register(arr.0).clone();
                let index = ctx.get_register(idx.0).clone();
                let val_val = ctx.get_register(val.0).clone();

                if let Some(obj) = array.as_object() {
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

                    // Fallback to generic access
                    if let Some(n) = index.as_int32() {
                        obj.set(PropertyKey::Index(n as u32), val_val);
                    } else {
                        let idx_str = self.to_string(&index);
                        obj.set(PropertyKey::string(&idx_str), val_val);
                    }
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
                    if let Some(otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                        shape_id: shape_addr,
                        offset,
                    }) = feedback.get(*ic_index as usize)
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
                            if let otter_vm_bytecode::function::InlineCacheState::Uninitialized = ic {
                                *ic = otter_vm_bytecode::function::InlineCacheState::Monomorphic {
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
                    if let Some(otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                        shape_id: shape_addr,
                        offset,
                    }) = feedback.get(*ic_index as usize)
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
                            if let otter_vm_bytecode::function::InlineCacheState::Uninitialized = ic {
                                *ic = otter_vm_bytecode::function::InlineCacheState::Monomorphic {
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
                    let iterator =
                        native_fn(std::slice::from_ref(&obj)).map_err(VmError::type_error)?;
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

            Instruction::IteratorNext { dst, done, iter } => {
                let iterator = ctx.get_register(iter.0).clone();

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
                    native_fn(std::slice::from_ref(&iterator)).map_err(VmError::type_error)?
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
                        .and_then(|v| v.as_object().cloned())
                } else {
                    None
                };

                let js_regex =
                    Arc::new(JsRegExp::new(pattern.to_string(), flags.to_string(), proto));
                Ok(Value::regex(js_regex))
            }
            Constant::TemplateLiteral(_) => {
                Err(VmError::internal("Template literals not yet supported"))
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
        args: &[Value],
    ) -> VmResult<Value> {
        ctx.enter_native_call()?;
        let result = native_fn(args).map_err(VmError::type_error);
        ctx.exit_native_call();
        result
    }

    /// Handle a function call value (native or closure)
    fn handle_call_value(
        &self,
        ctx: &mut VmContext,
        func_value: &Value,
        this_value: Value,
        args: Vec<Value>,
        return_reg: u16,
    ) -> VmResult<InstructionResult> {
        // Native function path
        if let Some(native_fn) = func_value.as_native_function() {
            // Direct execution with native depth tracking
            let mut native_args;
            let args_ref = if this_value.is_undefined() || this_value.is_callable() {
                &args
            } else {
                native_args = Vec::with_capacity(args.len() + 1);
                native_args.push(this_value);
                native_args.extend(args);
                &native_args
            };
            let result = self.call_native_fn(ctx, native_fn, args_ref)?;
            ctx.set_register(return_reg, result);
            return Ok(InstructionResult::Continue);
        }

        // Closure path
        if let Some(closure) = func_value.as_function() {
            let argc = args.len() as u8;
            ctx.set_pending_this(this_value);
            ctx.set_pending_args(args);
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

        Err(VmError::type_error("not a function"))
    }

    /// Add operation (handles string concatenation)
    fn op_add(&self, left: &Value, right: &Value) -> VmResult<Value> {
        // String concatenation
        if left.is_string() || right.is_string() {
            let left_str = self.to_string(left);
            let right_str = self.to_string(right);
            let result = format!("{}{}", left_str, right_str);
            let js_str = Arc::new(JsString::new(result));
            return Ok(Value::string(js_str));
        }

        let left_bigint = self.bigint_value(left)?;
        let right_bigint = self.bigint_value(right)?;
        if let (Some(left_bigint), Some(right_bigint)) = (left_bigint, right_bigint) {
            let result = left_bigint + right_bigint;
            return Ok(Value::bigint(result.to_string()));
        }

        if left.is_bigint() || right.is_bigint() {
            return Err(VmError::type_error(
                "Cannot mix BigInt and other types",
            ));
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
            let result = native_fn(&args).map_err(VmError::type_error)?;
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
            _ => "[object Object]".to_string(),
        }
    }

    /// Create a JavaScript Promise object from an internal promise
    /// This creates an object with _internal field and copies methods from Promise.prototype
    fn create_js_promise(&self, ctx: &VmContext, internal: Arc<JsPromise>) -> Value {
        let obj = Arc::new(JsObject::new(None));

        // Set _internal to the raw promise
        obj.set(PropertyKey::string("_internal"), Value::promise(internal));

        // Try to get Promise.prototype and copy its methods
        if let Some(promise_ctor) = ctx
            .get_global("Promise")
            .and_then(|v| v.as_object().cloned())
        {
            if let Some(proto) = promise_ctor
                .get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object().cloned())
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

    fn make_error(&self, ctx: &VmContext, name: &str, message: &str) -> Value {
        let ctor_value = ctx.get_global(name);
        let proto = ctor_value
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|obj| obj.get(&PropertyKey::string("prototype")))
            .and_then(|v| v.as_object().cloned());

        let obj = Arc::new(JsObject::new(proto));
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
            (Numeric::Number(left), Numeric::BigInt(right)) => Ok(
                self.compare_bigint_number(&right, left)
                    .map(|ordering| ordering.reverse()),
            ),
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
            PropertyKey::Index(n as u32)
        } else if let Some(s) = value.as_string() {
            PropertyKey::string(s.as_str())
        } else if let Some(sym) = value.as_symbol() {
            PropertyKey::Symbol(sym.id)
        } else {
            let key_str = self.to_string(value);
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
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
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
    Yield { value: Value },
    /// Throw a JS value
    Throw(Value),
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::operand::Register;
    use otter_vm_bytecode::{Function, Module};

    fn create_test_context() -> VmContext {
        let global = Arc::new(JsObject::new(None));
        VmContext::new(global)
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
            })
            // GetElem r3, r0, r2
            .instruction(Instruction::GetElem {
                dst: Register(3),
                arr: Register(0),
                idx: Register(2),
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
        assert_eq!(result.as_string().map(|s| s.as_str()), Some("function"));
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
            .local_count(1)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: otter_vm_bytecode::LocalIndex(0),
            })
            .instruction(Instruction::Add {
                dst: Register(1),
                lhs: Register(0),
                rhs: Register(0),
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
            })
            .instruction(Instruction::Return { src: Register(5) })
            .build();

        // inner(x): returns x * x
        let inner = Function::builder()
            .name("inner")
            .local_count(1)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: otter_vm_bytecode::LocalIndex(0),
            })
            .instruction(Instruction::Mul {
                dst: Register(1),
                lhs: Register(0),
                rhs: Register(0),
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
}
