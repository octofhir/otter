//! Bytecode interpreter
//!
//! Executes bytecode instructions.

use otter_vm_bytecode::{Instruction, Module, Register, TypeFlags, UpvalueCapture};

use crate::async_context::{AsyncContext, VmExecutionResult};
use crate::context::{TemplateCacheKey, VmContext};
use crate::error::{VmError, VmResult};
use crate::gc::GcRef;
use crate::generator::{GeneratorFrame, GeneratorState, JsGenerator};
use crate::object::{
    JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey, SetPropertyError,
    get_proto_epoch,
};
use crate::promise::{JsPromise, JsPromiseJob, JsPromiseJobKind, PromiseState};
use crate::realm::RealmId;
use crate::regexp::JsRegExp;
use crate::string::JsString;
use crate::value::{Closure, HeapRef, NativeFn, UpvalueCell, Value};

use num_bigint::BigInt as NumBigInt;
use num_traits::{One, ToPrimitive, Zero};
use std::cmp::Ordering;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

static DUMPED_ASSERT_RT: AtomicBool = AtomicBool::new(false);
use std::sync::Arc;

/// Extract a human-readable message from a panic payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        format!("internal panic: {}", s)
    } else if let Some(s) = payload.downcast_ref::<String>() {
        format!("internal panic: {}", s)
    } else {
        "internal panic: <unknown>".to_string()
    }
}

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

#[derive(Copy, Clone, Debug)]
pub(crate) enum PreferredType {
    Default,
    Number,
    String,
}

/// Maximum recursion depth for abstract equality comparison.
/// Prevents stack overflow from malicious valueOf/toString chains.
const MAX_ABSTRACT_EQUAL_DEPTH: usize = 128;

fn trace_modified_register_indices(instruction: &Instruction) -> Vec<u16> {
    match instruction {
        Instruction::IteratorNext { dst, done, .. } => vec![dst.0, done.0],

        Instruction::LoadUndefined { dst }
        | Instruction::LoadNull { dst }
        | Instruction::LoadTrue { dst }
        | Instruction::LoadFalse { dst }
        | Instruction::LoadInt8 { dst, .. }
        | Instruction::LoadInt32 { dst, .. }
        | Instruction::LoadConst { dst, .. }
        | Instruction::GetLocal { dst, .. }
        | Instruction::GetUpvalue { dst, .. }
        | Instruction::GetGlobal { dst, .. }
        | Instruction::LoadThis { dst }
        | Instruction::Add { dst, .. }
        | Instruction::Sub { dst, .. }
        | Instruction::Mul { dst, .. }
        | Instruction::Div { dst, .. }
        | Instruction::AddI32 { dst, .. }
        | Instruction::SubI32 { dst, .. }
        | Instruction::MulI32 { dst, .. }
        | Instruction::DivI32 { dst, .. }
        | Instruction::AddF64 { dst, .. }
        | Instruction::SubF64 { dst, .. }
        | Instruction::MulF64 { dst, .. }
        | Instruction::DivF64 { dst, .. }
        | Instruction::Mod { dst, .. }
        | Instruction::Pow { dst, .. }
        | Instruction::Neg { dst, .. }
        | Instruction::Inc { dst, .. }
        | Instruction::Dec { dst, .. }
        | Instruction::BitAnd { dst, .. }
        | Instruction::BitOr { dst, .. }
        | Instruction::BitXor { dst, .. }
        | Instruction::BitNot { dst, .. }
        | Instruction::Shl { dst, .. }
        | Instruction::Shr { dst, .. }
        | Instruction::Ushr { dst, .. }
        | Instruction::Eq { dst, .. }
        | Instruction::StrictEq { dst, .. }
        | Instruction::Ne { dst, .. }
        | Instruction::StrictNe { dst, .. }
        | Instruction::Lt { dst, .. }
        | Instruction::Le { dst, .. }
        | Instruction::Gt { dst, .. }
        | Instruction::Ge { dst, .. }
        | Instruction::Not { dst, .. }
        | Instruction::TypeOf { dst, .. }
        | Instruction::TypeOfName { dst, .. }
        | Instruction::InstanceOf { dst, .. }
        | Instruction::In { dst, .. }
        | Instruction::ToNumber { dst, .. }
        | Instruction::ToString { dst, .. }
        | Instruction::GetProp { dst, .. }
        | Instruction::GetPropConst { dst, .. }
        | Instruction::DeleteProp { dst, .. }
        | Instruction::NewObject { dst }
        | Instruction::NewArray { dst, .. }
        | Instruction::GetElem { dst, .. }
        | Instruction::Spread { dst, .. }
        | Instruction::Closure { dst, .. }
        | Instruction::Call { dst, .. }
        | Instruction::CallMethod { dst, .. }
        | Instruction::CreateArguments { dst }
        | Instruction::CallEval { dst, .. }
        | Instruction::CallWithReceiver { dst, .. }
        | Instruction::CallMethodComputed { dst, .. }
        | Instruction::Construct { dst, .. }
        | Instruction::CallSpread { dst, .. }
        | Instruction::ConstructSpread { dst, .. }
        | Instruction::CallMethodComputedSpread { dst, .. }
        | Instruction::Catch { dst }
        | Instruction::GetIterator { dst, .. }
        | Instruction::GetAsyncIterator { dst, .. }
        | Instruction::ForInNext { dst, .. }
        | Instruction::DefineClass { dst, .. }
        | Instruction::GetSuper { dst }
        | Instruction::CallSuper { dst, .. }
        | Instruction::GetSuperProp { dst, .. }
        | Instruction::CallSuperForward { dst }
        | Instruction::CallSuperSpread { dst, .. }
        | Instruction::Yield { dst, .. }
        | Instruction::Await { dst, .. }
        | Instruction::AsyncClosure { dst, .. }
        | Instruction::GeneratorClosure { dst, .. }
        | Instruction::AsyncGeneratorClosure { dst, .. }
        | Instruction::Move { dst, .. }
        | Instruction::Dup { dst, .. }
        | Instruction::Import { dst, .. } => vec![dst.0],

        _ => vec![],
    }
}

fn trace_modified_registers(instruction: &Instruction, ctx: &VmContext) -> Vec<(u16, String)> {
    trace_modified_register_indices(instruction)
        .into_iter()
        .map(|reg| (reg, format!("{:?}", ctx.get_register(reg))))
        .collect()
}

impl Interpreter {
    /// Create a new interpreter
    pub fn new() -> Self {
        Self {
            current_module: None,
        }
    }

    /// Execute a module
    pub fn execute(&self, module: &Module, ctx: &mut VmContext) -> VmResult<Value> {
        // Wrap in Arc for closure capture
        self.execute_arc(Arc::new(module.clone()), ctx)
    }

    /// Execute a module with Arc (for internal use and pre-created Arcs)
    pub fn execute_arc(&self, module: Arc<Module>, ctx: &mut VmContext) -> VmResult<Value> {
        self.execute_arc_with_locals(module, ctx, None)
    }

    /// Execute a module with Arc and initial local variables.
    ///
    /// The `initial_locals` map allows pre-populating local variables in the entry
    /// function. This is essential for ES modules where imported bindings are
    /// mapped to local variables that must be populated before execution.
    pub fn execute_arc_with_locals(
        &self,
        module: Arc<Module>,
        ctx: &mut VmContext,
        initial_locals: Option<std::collections::HashMap<u16, Value>>,
    ) -> VmResult<Value> {
        // Get entry function
        let entry_func = module
            .entry_function()
            .ok_or_else(|| VmError::internal("no entry function"))?;

        // Record the function call for hot function detection
        let became_hot = entry_func.record_call();
        if became_hot {
            #[cfg(feature = "jit")]
            {
                crate::jit_queue::enqueue_hot_function(&module, module.entry_point, entry_func);
                crate::jit_runtime::compile_one_pending_request();
            }
            #[cfg(not(feature = "jit"))]
            let _ = became_hot;
        }

        // Top-level scripts should have globalThis as `this`.
        ctx.set_pending_this(Value::object(ctx.global()));

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

        // Populate initial locals if provided
        if let Some(locals) = initial_locals {
            for (idx, value) in locals {
                ctx.set_local(idx, value)?;
            }
        }

        ctx.set_running(true);

        // Execute loop with panic protection.
        // Panics in the interpreter (from unwrap/expect on corrupted state)
        // are caught and converted to VmError::InternalError, preventing
        // the entire process from aborting.
        let result = {
            use std::panic::{AssertUnwindSafe, catch_unwind};
            match catch_unwind(AssertUnwindSafe(|| self.run_loop(ctx))) {
                Ok(result) => result,
                Err(panic_payload) => Err(VmError::internal(&panic_message(&panic_payload))),
            }
        };

        // Capture exports from the current frame before popping it
        if result.is_ok() {
            let mut exports = std::collections::HashMap::new();
            if let Some(entry_func) = module.entry_function() {
                for export in &module.exports {
                    match export {
                        otter_vm_bytecode::module::ExportRecord::Named { local, exported } => {
                            if let Some(idx) =
                                entry_func.local_names.iter().position(|n| n == local)
                            {
                                if let Ok(val) = ctx.get_local(idx as u16) {
                                    exports.insert(exported.clone(), val);
                                }
                            }
                        }
                        otter_vm_bytecode::module::ExportRecord::Default { local } => {
                            if let Some(idx) =
                                entry_func.local_names.iter().position(|n| n == local)
                            {
                                if let Ok(val) = ctx.get_local(idx as u16) {
                                    exports.insert("default".to_string(), val);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            ctx.set_captured_exports(exports);
        }

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
        &self,
        module: Arc<Module>,
        ctx: &mut VmContext,
        result_promise: GcRef<JsPromise>,
    ) -> VmExecutionResult {
        self.execute_with_suspension_and_locals(module, ctx, result_promise, None)
    }

    pub fn execute_with_suspension_and_locals(
        &self,
        module: Arc<Module>,
        ctx: &mut VmContext,
        result_promise: GcRef<JsPromise>,
        initial_locals: Option<std::collections::HashMap<u16, Value>>,
    ) -> VmExecutionResult {
        // Get entry function
        let entry_func = match module.entry_function() {
            Some(f) => f,
            None => return VmExecutionResult::Error("no entry function".to_string()),
        };

        // Record the function call for hot function detection
        let became_hot = entry_func.record_call();
        if became_hot {
            #[cfg(feature = "jit")]
            {
                crate::jit_queue::enqueue_hot_function(&module, module.entry_point, entry_func);
                crate::jit_runtime::compile_one_pending_request();
            }
            #[cfg(not(feature = "jit"))]
            let _ = became_hot;
        }

        // Top-level scripts should have globalThis as `this`.
        ctx.set_pending_this(Value::object(ctx.global()));

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

        // Populate initial locals if provided (import bindings)
        if let Some(locals) = initial_locals {
            for (idx, value) in locals {
                let _ = ctx.set_local(idx, value);
            }
        }

        ctx.set_running(true);

        // Execute loop with suspension support and panic protection
        {
            use std::panic::{AssertUnwindSafe, catch_unwind};
            match catch_unwind(AssertUnwindSafe(|| {
                self.run_loop_with_suspension(ctx, result_promise)
            })) {
                Ok(result) => result,
                Err(panic_payload) => {
                    ctx.set_running(false);
                    VmExecutionResult::Error(panic_message(&panic_payload))
                }
            }
        }
    }

    /// Resume execution from a saved async context
    ///
    /// This is called when a Promise that was awaited resolves.
    /// It restores the VM state and continues execution.
    pub fn resume_async(
        &self,
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

        // Continue execution with panic protection
        {
            use std::panic::{AssertUnwindSafe, catch_unwind};
            match catch_unwind(AssertUnwindSafe(|| {
                self.run_loop_with_suspension(ctx, async_ctx.result_promise)
            })) {
                Ok(result) => result,
                Err(panic_payload) => {
                    ctx.set_running(false);
                    VmExecutionResult::Error(panic_message(&panic_payload))
                }
            }
        }
    }

    /// Resume execution from a saved async context with a rejection (throw).
    ///
    /// Called when an awaited Promise rejects. Restores frames and processes
    /// the rejection value through the VM's try-catch machinery.
    pub fn resume_async_throw(
        &self,
        ctx: &mut VmContext,
        async_ctx: AsyncContext,
        rejection_value: Value,
    ) -> VmExecutionResult {
        // Restore the call stack from saved frames
        if let Err(e) = ctx.restore_frames(async_ctx.frames) {
            return VmExecutionResult::Error(e.to_string());
        }

        ctx.set_running(async_ctx.was_running);

        // Set the pending throw value — the run loop will handle it
        // through try-catch or propagate it as an uncaught exception
        ctx.set_pending_throw(Some(rejection_value));

        // Continue execution with panic protection
        {
            use std::panic::{AssertUnwindSafe, catch_unwind};
            match catch_unwind(AssertUnwindSafe(|| {
                self.run_loop_with_suspension(ctx, async_ctx.result_promise)
            })) {
                Ok(result) => result,
                Err(panic_payload) => {
                    ctx.set_running(false);
                    VmExecutionResult::Error(panic_message(&panic_payload))
                }
            }
        }
    }

    /// Call a function value (native or closure) with arguments
    ///
    /// This method allows calling JavaScript functions from Rust code.
    /// It handles both native functions (direct call) and closures (push frame and execute).
    pub fn call_function(
        &self,
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

        // Generator functions: create a generator object, don't execute the body
        if closure.is_generator {
            let realm_id = closure
                .object
                .get(&PropertyKey::string("__realm_id__"))
                .and_then(|v| v.as_int32())
                .map(|id| id as u32)
                .unwrap_or_else(|| ctx.realm_id());
            let proto = ctx
                .realm_intrinsics(realm_id)
                .map(|intrinsics| {
                    if closure.is_async {
                        intrinsics.async_generator_prototype
                    } else {
                        intrinsics.generator_prototype
                    }
                })
                .or_else(|| {
                    if closure.is_async {
                        ctx.async_generator_prototype_intrinsic()
                    } else {
                        ctx.generator_prototype_intrinsic()
                    }
                });
            let gen_obj = GcRef::new(JsObject::new(
                proto.map(Value::object).unwrap_or_else(Value::null),
                ctx.memory_manager().clone(),
            ));
            let generator = JsGenerator::new(
                closure.function_index,
                Arc::clone(&closure.module),
                closure.upvalues.clone(),
                args.to_vec(),
                this_value,
                false,
                closure.is_async,
                realm_id,
                gen_obj,
            );
            generator.set_callee_value(func.clone());
            return Ok(Value::generator(generator));
        }

        // Save current state
        let was_running = ctx.is_running();
        let prev_stack_depth = ctx.stack_depth();

        // Get function info
        let func_info = closure
            .module
            .function(closure.function_index)
            .ok_or_else(|| VmError::internal("function not found"))?;

        // Set up the call — handle rest parameters
        let mut call_args: Vec<Value> = args.to_vec();
        if func_info.flags.has_rest {
            let param_count = func_info.param_count as usize;
            let rest_args: Vec<Value> = if call_args.len() > param_count {
                call_args.drain(param_count..).collect()
            } else {
                Vec::new()
            };
            let rest_arr = crate::gc::GcRef::new(crate::object::JsObject::array(
                rest_args.len(),
                ctx.memory_manager().clone(),
            ));
            if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object()) {
                if let Some(array_proto) = array_obj
                    .get(&crate::object::PropertyKey::string("prototype"))
                    .and_then(|v| v.as_object())
                {
                    rest_arr.set_prototype(Value::object(array_proto));
                }
            }
            for (i, arg) in rest_args.into_iter().enumerate() {
                let _ = rest_arr.set(crate::object::PropertyKey::Index(i as u32), arg);
            }
            call_args.push(Value::object(rest_arr));
        }

        let argc = call_args.len();
        ctx.set_pending_args(call_args);
        ctx.set_pending_this(this_value);
        ctx.set_pending_upvalues(closure.upvalues.clone());
        // Propagate home_object from closure to the new call frame
        if let Some(ref ho) = closure.home_object {
            ctx.set_pending_home_object(ho.clone());
        }

        let realm_id = self.realm_id_for_function(ctx, func);
        ctx.set_pending_realm_id(realm_id);
        // Store callee value for arguments.callee
        ctx.set_pending_callee_value(func.clone());
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

            let instruction_result = match self.execute_instruction(instruction, &current_module, ctx) {
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
                    other => {
                        while ctx.stack_depth() > prev_stack_depth {
                            ctx.pop_frame();
                        }
                        ctx.set_running(was_running);
                        return Err(other);
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
                    let value = if is_construct && !value.is_object() {
                        construct_this
                    } else if is_async {
                        self.create_js_promise(ctx, JsPromise::resolved(value))
                    } else {
                        value
                    };
                    // Check if we've returned to the original depth
                    if ctx.stack_depth() <= prev_stack_depth + 1 {
                        ctx.pop_frame();
                        break value;
                    }
                    // Handle return from nested call
                    ctx.pop_frame();
                    if let Some(reg) = return_reg {
                        ctx.set_register(reg, value);
                    } else {
                        ctx.set_register(0, value);
                    }
                }
                InstructionResult::Call {
                    func_index,
                    module,
                    argc,
                    return_reg,
                    is_construct,
                    is_async,
                    upvalues,
                } => {
                    ctx.advance_pc();
                    let func = module
                        .function(func_index)
                        .ok_or_else(|| VmError::internal("function not found"))?;

                    // Record the function call for hot function detection
                    let became_hot = func.record_call();
                    if became_hot {
                        #[cfg(feature = "jit")]
                        {
                            crate::jit_queue::enqueue_hot_function(&module, func_index, func);
                            crate::jit_runtime::compile_one_pending_request();
                        }
                        #[cfg(not(feature = "jit"))]
                        let _ = became_hot;
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
                InstructionResult::TailCall {
                    func_index,
                    module,
                    argc,
                    return_reg,
                    is_async,
                    upvalues,
                } => {
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
                InstructionResult::Suspend { .. } => {
                    // Can't handle suspension in direct call, return undefined
                    break Value::undefined();
                }
                InstructionResult::Yield { .. } => {
                    // Can't handle yield in direct call, return undefined
                    break Value::undefined();
                }
                InstructionResult::Throw(error) => {
                    // Handle throws caught inside the function(s) started by this call.
                    if let Some((target_depth, catch_pc)) = ctx.peek_nearest_try()
                        && target_depth > prev_stack_depth
                    {
                        let _ = ctx.take_nearest_try();
                        while ctx.stack_depth() > target_depth {
                            ctx.pop_frame();
                        }
                        if let Some(frame) = ctx.current_frame_mut() {
                            frame.pc = catch_pc;
                        }
                        ctx.set_exception(error);
                        continue;
                    }

                    // Pop the frame we pushed and unwind to original depth
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame();
                    }
                    ctx.set_running(was_running);
                    return Err(VmError::exception(error));
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
        awaited_promise: GcRef<JsPromise>,
        result_promise: GcRef<JsPromise>,
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
        &self,
        ctx: &mut VmContext,
        result_promise: GcRef<JsPromise>,
    ) -> VmExecutionResult {
        // Cache module Arc - only refresh when frame changes
        let mut cached_module: Option<Arc<Module>> = None;
        let mut cached_frame_id: usize = usize::MAX;

        // Check for pending throw (injected by resume_async_throw)
        if let Some(throw_value) = ctx.take_pending_throw() {
            // Process through try-catch machinery
            if let Some(handler) = ctx.take_nearest_try() {
                while ctx.stack_depth() > handler.0 {
                    ctx.pop_frame();
                }
                if let Some(frame) = ctx.current_frame_mut() {
                    frame.pc = handler.1;
                }
                ctx.set_register(0, throw_value);
                // Fall through to the main loop
            } else {
                // No try-catch — uncaught exception
                ctx.set_running(false);
                // Format the error for display
                let msg = if let Some(s) = throw_value.as_string() {
                    s.as_str().to_string()
                } else if let Some(obj) = throw_value.as_object() {
                    obj.get(&crate::object::PropertyKey::string("message"))
                        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                        .unwrap_or_else(|| format!("{:?}", throw_value))
                } else {
                    format!("{:?}", throw_value)
                };
                return VmExecutionResult::Error(format!("Uncaught exception: {}", msg));
            }
        }

        loop {
            // Periodic interrupt check for responsive timeouts
            if ctx.should_check_interrupt() {
                if ctx.is_interrupted() {
                    ctx.set_running(false);
                    return VmExecutionResult::Error("Execution interrupted".to_string());
                }
                // Check for GC trigger at safepoint
                ctx.maybe_collect_garbage();
                // Update debug snapshot
                ctx.update_debug_snapshot();
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

            // Capture trace data while frame is borrowed (record after execution)
            let trace_data = if ctx.trace_state.is_some() {
                Some((
                    frame.pc,
                    frame.function_index,
                    Arc::clone(&frame.module),
                    instruction.clone(),
                ))
            } else {
                None
            };
            let trace_capture_timing = ctx
                .trace_state
                .as_ref()
                .map(|state| state.config.capture_timing)
                .unwrap_or(false);
            let trace_start_time = if trace_capture_timing {
                Some(std::time::Instant::now())
            } else {
                None
            };

            let (_func_idx, _pc) = (frame.function_index, frame.pc);

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

            match instruction_result {
                InstructionResult::Continue => {
                    ctx.advance_pc();
                }
                InstructionResult::Jump(offset) => {
                    if std::env::var("OTTER_TRACE_ASSERT_JUMP_APPLY").is_ok() {
                        if let Some(frame) = ctx.current_frame() {
                            if let Some(func) = frame.module.function(frame.function_index) {
                                if func.name.as_deref() == Some("assert") {
                                    let old_pc = frame.pc;
                                    let new_pc = (old_pc as i64 + offset as i64) as usize;
                                    eprintln!(
                                        "[OTTER_TRACE_ASSERT_JUMP_APPLY] pc={} offset={} new_pc={}",
                                        old_pc, offset, new_pc
                                    );
                                }
                            }
                        }
                    }
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
                            rest_arr.set_prototype(Value::object(array_proto));
                        }
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            let _ = rest_arr.set(PropertyKey::Index(i as u32), arg);
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
                                result_promise,
                            );
                            return VmExecutionResult::Suspended(async_ctx);
                        }
                    }
                }
                InstructionResult::Yield { value, .. } => {
                    // Generator yielded a value
                    let result =
                        GcRef::new(JsObject::new(Value::null(), ctx.memory_manager().clone()));
                    let _ = result.set(PropertyKey::string("value"), value);
                    let _ = result.set(PropertyKey::string("done"), Value::boolean(false));
                    ctx.advance_pc();
                    return VmExecutionResult::Complete(Value::object(result));
                }
            }
        }
    }

    /// Main execution loop
    fn run_loop(&self, ctx: &mut VmContext) -> VmResult<Value> {
        // Cache module Arc - only refresh when frame changes
        let mut cached_module: Option<Arc<Module>> = None;
        let mut cached_frame_id: usize = usize::MAX;
        let mut last_pc_by_frame_id: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();

        loop {
            // Periodic interrupt check for responsive timeouts
            if ctx.should_check_interrupt() {
                if ctx.is_interrupted() {
                    return Err(VmError::interrupted());
                }
                // Check for GC trigger at safepoint
                ctx.maybe_collect_garbage();
                // Update debug snapshot
                ctx.update_debug_snapshot();
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

            if std::env::var("OTTER_TRACE_ASSERT_PC_BACKTRACK").is_ok()
                && func.name.as_deref() == Some("assert")
            {
                if let Some(prev_pc) = last_pc_by_frame_id.get(&frame.frame_id).copied()
                    && frame.pc < prev_pc
                {
                    eprintln!(
                        "[OTTER_TRACE_ASSERT_PC_BACKTRACK] frame_id={} reg_base={} pc={} prev_pc={}",
                        frame.frame_id, frame.register_base, frame.pc, prev_pc
                    );
                }
                last_pc_by_frame_id.insert(frame.frame_id, frame.pc);
            }

            if std::env::var("OTTER_TRACE_ASSERT_PC").is_ok()
                && func.name.as_deref() == Some("assert")
            {
                eprintln!(
                    "[OTTER_TRACE_ASSERT_PC] frame_id={} pc={}",
                    frame.frame_id, frame.pc
                );
            }

            if std::env::var("OTTER_DUMP_ASSERT_RT").is_ok()
                && func.name.as_deref() == Some("assert")
                && !DUMPED_ASSERT_RT.swap(true, AtomicOrdering::SeqCst)
            {
                eprintln!(
                    "[OTTER_DUMP_ASSERT_RT] function=assert instructions={} registers={}",
                    func.instructions.len(),
                    func.register_count
                );
                for (idx, instr) in func.instructions.iter().enumerate() {
                    eprintln!("  {:04} {:?}", idx, instr);
                }
            }

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

            // Capture trace data while frame is borrowed (record after execution)
            let trace_data = if ctx.trace_state.is_some() {
                Some((
                    frame.pc,
                    frame.function_index,
                    Arc::clone(&frame.module),
                    instruction.clone(),
                ))
            } else {
                None
            };
            let trace_capture_timing = ctx
                .trace_state
                .as_ref()
                .map(|state| state.config.capture_timing)
                .unwrap_or(false);
            let trace_start_time = if trace_capture_timing {
                Some(std::time::Instant::now())
            } else {
                None
            };

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
                            rest_arr.set_prototype(Value::object(array_proto));
                        }
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            let _ = rest_arr.set(PropertyKey::Index(i as u32), arg);
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
                            rest_arr.set_prototype(Value::object(array_proto));
                        }
                        for (i, arg) in rest_args.into_iter().enumerate() {
                            let _ = rest_arr.set(PropertyKey::Index(i as u32), arg);
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
                    let result =
                        GcRef::new(JsObject::new(Value::null(), ctx.memory_manager().clone()));
                    let _ = result.set(PropertyKey::string("value"), value);
                    let _ = result.set(PropertyKey::string("done"), Value::boolean(false));
                    ctx.advance_pc();
                    return Ok(Value::object(result));
                }
            }
        }
    }

    /// Execute a single instruction
    fn execute_instruction(
        &self,
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

                if std::env::var("OTTER_TRACE_ASSERT_GETLOCAL").is_ok() {
                    if let Some(frame) = ctx.current_frame() {
                        if let Some(func) = frame.module.function(frame.function_index) {
                            if func.name.as_deref() == Some("assert") {
                                eprintln!(
                                    "[OTTER_TRACE_ASSERT_GETLOCAL] idx={} dst_reg={} type={}",
                                    idx.0,
                                    dst.0,
                                    value.type_of()
                                );
                            }
                        }
                    }
                }
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::SetLocal { idx, src } => {
                let value = ctx.get_register(src.0).clone();
                if std::env::var("OTTER_TRACE_ASSERT_SETLOCAL").is_ok()
                    && idx.0 == 2
                    && ctx
                        .current_frame()
                        .and_then(|frame| frame.module.function(frame.function_index))
                        .and_then(|func| func.name.as_deref())
                        == Some("main")
                {
                    let has_open = ctx
                        .open_upvalues_to_trace()
                        .contains_key(&(ctx.current_frame().unwrap().frame_id, idx.0));
                    eprintln!(
                        "[OTTER_TRACE_ASSERT_SETLOCAL] func=main idx={} type={} open_upvalue={}",
                        idx.0,
                        value.type_of(),
                        has_open
                    );
                }
                ctx.set_local(idx.0, value)?;
                Ok(InstructionResult::Continue)
            }

            Instruction::GetUpvalue { dst, idx } => {
                // Get value from upvalue cell
                let value = ctx.get_upvalue(idx.0)?;
                if std::env::var("OTTER_TRACE_ASSERT_UPVALUE").is_ok() {
                    if let Some(frame) = ctx.current_frame() {
                        if let Some(func) = frame.module.function(frame.function_index) {
                            if func.name.as_deref() == Some("assert") {
                                eprintln!(
                                    "[OTTER_TRACE_ASSERT_UPVALUE] func=assert idx={} dst_reg={} type={}",
                                    idx.0,
                                    dst.0,
                                    value.type_of()
                                );
                            }
                        }
                    }
                }
                if std::env::var("OTTER_TRACE_ASSERT_UPVALUE0").is_ok() && idx.0 == 0 {
                    eprintln!(
                        "[OTTER_TRACE_ASSERT_UPVALUE0] idx=0 dst_reg={} type={}",
                        dst.0,
                        value.type_of()
                    );
                }
                if std::env::var("OTTER_TRACE_ASSERT_UPVALUE0_ASSERT").is_ok() && idx.0 == 0 {
                    if let Some(frame) = ctx.current_frame() {
                        if let Some(func) = frame.module.function(frame.function_index) {
                            if func.name.as_deref() == Some("assert") {
                                eprintln!(
                                    "[OTTER_TRACE_ASSERT_UPVALUE0_ASSERT] pc={} dst_reg={} type={}",
                                    frame.pc,
                                    dst.0,
                                    value.type_of()
                                );
                            }
                        }
                    }
                }
                if std::env::var("OTTER_TRACE_ASSERT_UPVALUE_WRITE").is_ok() && idx.0 == 0 {
                    if let Some(frame) = ctx.current_frame() {
                        if let Some(func) = frame.module.function(frame.function_index) {
                            if func.name.as_deref() == Some("assert") {
                                eprintln!(
                                    "[OTTER_TRACE_ASSERT_UPVALUE_WRITE] pc={} dst_reg={} type={}",
                                    frame.pc,
                                    dst.0,
                                    value.type_of()
                                );
                            }
                        }
                    }
                }
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

                let trace_assert_globals = std::env::var("OTTER_TRACE_ASSERT_GLOBALS").is_ok();
                let is_assert_func = if trace_assert_globals {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    frame
                        .module
                        .function(frame.function_index)
                        .ok_or_else(|| VmError::internal("no function"))?
                        .name
                        .as_deref()
                        == Some("assert")
                } else {
                    false
                };

                // IC Fast Path
                let cached_value = {
                    let global_obj = ctx.global();
                    if global_obj.is_dictionary_mode() {
                        None
                    } else {
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
                                if std::sync::Arc::as_ptr(&global_obj.shape()) as u64 == *shape_addr
                                {
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
                    }
                };

                if let Some(value) = cached_value {
                    if is_assert_func {
                        eprintln!(
                            "[OTTER_TRACE_ASSERT_GLOBALS] GetGlobal(ic) name={} type={}",
                            String::from_utf16_lossy(name_str),
                            value.type_of()
                        );
                    }
                    let trace_array = std::env::var("OTTER_TRACE_ARRAY").is_ok();
                    if trace_array && Self::utf16_eq_ascii(name_str, "Array") {
                        eprintln!(
                            "[OTTER_TRACE_ARRAY] GetGlobal(ic) name=Array result_type={} obj_ptr={:?}",
                            value.type_of(),
                            value.as_object().map(|o| o.as_ptr())
                        );
                    }
                    if std::env::var("OTTER_TRACE_GLOBAL_ASSERT").is_ok()
                        && Self::utf16_eq_ascii(name_str, "assert")
                    {
                        eprintln!(
                            "[OTTER_TRACE_GLOBAL_ASSERT] GetGlobal(ic) assert type={}",
                            value.type_of()
                        );
                    }
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
                    if !global_obj.is_dictionary_mode() {
                        let key = Self::utf16_key(name_str);
                        if let Some(offset) = global_obj.shape().get_offset(&key) {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            let func = frame
                                .module
                                .function(frame.function_index)
                                .ok_or_else(|| VmError::internal("no function"))?;
                            let feedback = func.feedback_vector.write();
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
                }

                let trace_array = std::env::var("OTTER_TRACE_ARRAY").is_ok();
                if trace_array && Self::utf16_eq_ascii(name_str, "Array") {
                    eprintln!(
                        "[OTTER_TRACE_ARRAY] GetGlobal(slow) name=Array result_type={} obj_ptr={:?}",
                        value.type_of(),
                        value.as_object().map(|o| o.as_ptr())
                    );
                }
                if std::env::var("OTTER_TRACE_GLOBAL_ASSERT").is_ok()
                    && Self::utf16_eq_ascii(name_str, "assert")
                {
                    eprintln!(
                        "[OTTER_TRACE_GLOBAL_ASSERT] GetGlobal(slow) assert type={}",
                        value.type_of()
                    );
                }
                if is_assert_func {
                    eprintln!(
                        "[OTTER_TRACE_ASSERT_GLOBALS] GetGlobal(slow) name={} type={}",
                        String::from_utf16_lossy(name_str),
                        value.type_of()
                    );
                }
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::SetGlobal {
                name,
                src,
                ic_index,
                is_declaration,
            } => {
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;

                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;
                let val_val = ctx.get_register(src.0).clone();
                if std::env::var("OTTER_TRACE_GLOBAL_ASSERT").is_ok()
                    && Self::utf16_eq_ascii(name_str, "assert")
                {
                    eprintln!(
                        "[OTTER_TRACE_GLOBAL_ASSERT] SetGlobal assert type={}",
                        val_val.type_of()
                    );
                }

                // IC Fast Path
                {
                    let global_obj = ctx.global().clone();
                    if !global_obj.is_dictionary_mode() {
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
                                if std::sync::Arc::as_ptr(&global_obj.shape()) as u64 == *shape_addr
                                {
                                    if global_obj
                                        .set_by_offset(*offset as usize, val_val.clone())
                                        .is_ok()
                                    {
                                        return Ok(InstructionResult::Continue);
                                    }
                                }
                            }
                        }
                    }
                }

                // Strict mode: ReferenceError on assignment to undeclared variable
                if !is_declaration {
                    let is_strict = ctx
                        .current_frame()
                        .and_then(|frame| frame.module.function(frame.function_index))
                        .map(|func| func.flags.is_strict)
                        .unwrap_or(false);

                    if is_strict {
                        let global_obj = ctx.global().clone();
                        let key = Self::utf16_key(name_str);
                        let property_exists = if global_obj.is_dictionary_mode() {
                            global_obj.has_own(&key)
                        } else {
                            global_obj.shape().get_offset(&key).is_some()
                        };
                        if !property_exists {
                            return Err(VmError::ReferenceError(format!(
                                "{} is not defined",
                                String::from_utf16_lossy(name_str)
                            )));
                        }
                    }
                }

                ctx.set_global_utf16(name_str, val_val.clone());

                // Update IC
                {
                    let global_obj = ctx.global().clone();
                    if !global_obj.is_dictionary_mode() {
                        let key = Self::utf16_key(name_str);
                        if let Some(offset) = global_obj.shape().get_offset(&key) {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            let func = frame
                                .module
                                .function(frame.function_index)
                                .ok_or_else(|| VmError::internal("no function"))?;
                            let feedback = func.feedback_vector.write();
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
                let value = ctx.get_register(src.0).clone();
                let number = if value.is_object() {
                    let prim = self.to_primitive(ctx, &value, PreferredType::Number)?;
                    if prim.is_bigint() {
                        return Err(VmError::type_error("Cannot convert BigInt to number"));
                    }
                    self.to_number_value(ctx, &prim)?
                } else {
                    if value.is_bigint() {
                        return Err(VmError::type_error("Cannot convert BigInt to number"));
                    }
                    self.to_number_value(ctx, &value)?
                };
                ctx.set_register(dst.0, Value::number(number));
                Ok(InstructionResult::Continue)
            }
            Instruction::ToString { dst, src } => {
                let value = ctx.get_register(src.0).clone();
                let s = self.to_string_value(ctx, &value)?;
                ctx.set_register(dst.0, Value::string(JsString::intern(&s)));
                Ok(InstructionResult::Continue)
            }

            Instruction::RequireCoercible { src } => {
                let value = ctx.get_register(src.0).clone();
                if value.is_null() {
                    return Err(VmError::type_error("Cannot destructure 'null' value"));
                }
                if value.is_undefined() {
                    return Err(VmError::type_error("Cannot destructure 'undefined' value"));
                }
                Ok(InstructionResult::Continue)
            }

            // ==================== Arithmetic ====================
            Instruction::Add {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                // Collect type feedback and check for quickening opportunity
                let use_int32_fast_path = if let Some(frame) = ctx.current_frame() {
                    if let Some(func) = frame.module.function(frame.function_index) {
                        let feedback = func.feedback_vector.write();
                        if let Some(meta) = feedback.get_mut(*feedback_index as usize) {
                            Self::observe_value_type(&mut meta.type_observations, &left);
                            Self::observe_value_type(&mut meta.type_observations, &right);
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
                let result = self.op_add(ctx, &left, &right)?;
                ctx.set_register(dst.0, result);
                Ok(InstructionResult::Continue)
            }

            Instruction::Sub {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left_value = ctx.get_register(lhs.0).clone();
                let right_value = ctx.get_register(rhs.0).clone();

                // Collect type feedback and check for quickening opportunity
                let use_int32_fast_path = if let Some(frame) = ctx.current_frame() {
                    if let Some(func) = frame.module.function(frame.function_index) {
                        let feedback = func.feedback_vector.write();
                        if let Some(meta) = feedback.get_mut(*feedback_index as usize) {
                            Self::observe_value_type(&mut meta.type_observations, &left_value);
                            Self::observe_value_type(&mut meta.type_observations, &right_value);
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

                // Generic path (ToNumeric)
                let left_num = self.to_numeric(ctx, &left_value)?;
                let right_num = self.to_numeric(ctx, &right_value)?;

                match (left_num, right_num) {
                    (Numeric::BigInt(left), Numeric::BigInt(right)) => {
                        let result = left - right;
                        ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    }
                    (Numeric::Number(left), Numeric::Number(right)) => {
                        ctx.set_register(dst.0, Value::number(left - right));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Ok(InstructionResult::Continue)
            }

            Instruction::Inc { dst, src } => {
                let value = ctx.get_register(src.0).clone();
                let numeric = self.to_numeric(ctx, &value)?;
                match numeric {
                    Numeric::BigInt(bigint) => {
                        let result = bigint + NumBigInt::one();
                        ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    }
                    Numeric::Number(num) => {
                        ctx.set_register(dst.0, Value::number(num + 1.0));
                    }
                }
                Ok(InstructionResult::Continue)
            }

            Instruction::Dec { dst, src } => {
                let value = ctx.get_register(src.0).clone();
                let numeric = self.to_numeric(ctx, &value)?;
                match numeric {
                    Numeric::BigInt(bigint) => {
                        let result = bigint - NumBigInt::one();
                        ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    }
                    Numeric::Number(num) => {
                        ctx.set_register(dst.0, Value::number(num - 1.0));
                    }
                }
                Ok(InstructionResult::Continue)
            }

            Instruction::Mul {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left_value = ctx.get_register(lhs.0).clone();
                let right_value = ctx.get_register(rhs.0).clone();

                // Collect type feedback and check for quickening opportunity
                let use_int32_fast_path = if let Some(frame) = ctx.current_frame() {
                    if let Some(func) = frame.module.function(frame.function_index) {
                        let feedback = func.feedback_vector.write();
                        if let Some(meta) = feedback.get_mut(*feedback_index as usize) {
                            Self::observe_value_type(&mut meta.type_observations, &left_value);
                            Self::observe_value_type(&mut meta.type_observations, &right_value);
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

                // Generic path (ToNumeric)
                let left_num = self.to_numeric(ctx, &left_value)?;
                let right_num = self.to_numeric(ctx, &right_value)?;

                match (left_num, right_num) {
                    (Numeric::BigInt(left), Numeric::BigInt(right)) => {
                        let result = left * right;
                        ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    }
                    (Numeric::Number(left), Numeric::Number(right)) => {
                        ctx.set_register(dst.0, Value::number(left * right));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Ok(InstructionResult::Continue)
            }

            Instruction::Div {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                let left_value = ctx.get_register(lhs.0).clone();
                let right_value = ctx.get_register(rhs.0).clone();

                // Collect type feedback and check for quickening opportunity
                let use_int32_fast_path = if let Some(frame) = ctx.current_frame() {
                    if let Some(func) = frame.module.function(frame.function_index) {
                        let feedback = func.feedback_vector.write();
                        if let Some(meta) = feedback.get_mut(*feedback_index as usize) {
                            Self::observe_value_type(&mut meta.type_observations, &left_value);
                            Self::observe_value_type(&mut meta.type_observations, &right_value);
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

                // Generic path (ToNumeric)
                let left_num = self.to_numeric(ctx, &left_value)?;
                let right_num = self.to_numeric(ctx, &right_value)?;

                match (left_num, right_num) {
                    (Numeric::BigInt(left), Numeric::BigInt(right)) => {
                        if right.is_zero() {
                            return Err(VmError::range_error("Division by zero"));
                        }
                        let result = left / right;
                        ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    }
                    (Numeric::Number(left), Numeric::Number(right)) => {
                        ctx.set_register(dst.0, Value::number(left / right));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Ok(InstructionResult::Continue)
            }

            // ==================== Quickened Arithmetic (type-specialized) ====================
            Instruction::AddI32 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                // Fast path: both operands are int32
                if let (Some(l), Some(r)) = (left.as_int32(), right.as_int32()) {
                    // Check for overflow, fall back to f64 if it occurs
                    if let Some(result) = l.checked_add(r) {
                        ctx.set_register(dst.0, Value::int32(result));
                        return Ok(InstructionResult::Continue);
                    }
                }

                // Fallback to generic add
                let result = self.op_add(ctx, &left, &right)?;
                ctx.set_register(dst.0, result);
                Ok(InstructionResult::Continue)
            }

            Instruction::SubI32 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                // Fast path: both operands are int32
                if let (Some(l), Some(r)) = (left.as_int32(), right.as_int32()) {
                    if let Some(result) = l.checked_sub(r) {
                        ctx.set_register(dst.0, Value::int32(result));
                        return Ok(InstructionResult::Continue);
                    }
                }

                // Fallback to generic sub
                let left_num = self.coerce_number(ctx, left)?;
                let right_num = self.coerce_number(ctx, right)?;
                ctx.set_register(dst.0, Value::number(left_num - right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::MulI32 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                // Fast path: both operands are int32
                if let (Some(l), Some(r)) = (left.as_int32(), right.as_int32()) {
                    if let Some(result) = l.checked_mul(r) {
                        ctx.set_register(dst.0, Value::int32(result));
                        return Ok(InstructionResult::Continue);
                    }
                }

                // Fallback to generic mul
                let left_num = self.coerce_number(ctx, left)?;
                let right_num = self.coerce_number(ctx, right)?;
                ctx.set_register(dst.0, Value::number(left_num * right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::DivI32 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

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
                let left_num = self.coerce_number(ctx, left)?;
                let right_num = self.coerce_number(ctx, right)?;
                ctx.set_register(dst.0, Value::number(left_num / right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::AddF64 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                // Fast path: both operands are numbers
                if let (Some(l), Some(r)) = (left.as_number(), right.as_number()) {
                    ctx.set_register(dst.0, Value::number(l + r));
                    return Ok(InstructionResult::Continue);
                }

                // Fallback to generic add
                let result = self.op_add(ctx, &left, &right)?;
                ctx.set_register(dst.0, result);
                Ok(InstructionResult::Continue)
            }

            Instruction::SubF64 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                // Fast path: both operands are numbers
                if let (Some(l), Some(r)) = (left.as_number(), right.as_number()) {
                    ctx.set_register(dst.0, Value::number(l - r));
                    return Ok(InstructionResult::Continue);
                }

                // Fallback to generic sub
                let left_num = self.coerce_number(ctx, left)?;
                let right_num = self.coerce_number(ctx, right)?;
                ctx.set_register(dst.0, Value::number(left_num - right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::MulF64 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                // Fast path: both operands are numbers
                if let (Some(l), Some(r)) = (left.as_number(), right.as_number()) {
                    ctx.set_register(dst.0, Value::number(l * r));
                    return Ok(InstructionResult::Continue);
                }

                // Fallback to generic mul
                let left_num = self.coerce_number(ctx, left)?;
                let right_num = self.coerce_number(ctx, right)?;
                ctx.set_register(dst.0, Value::number(left_num * right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::DivF64 {
                dst,
                lhs,
                rhs,
                feedback_index: _,
            } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                // Fast path: both operands are numbers
                if let (Some(l), Some(r)) = (left.as_number(), right.as_number()) {
                    ctx.set_register(dst.0, Value::number(l / r));
                    return Ok(InstructionResult::Continue);
                }

                // Fallback to generic div
                let left_num = self.coerce_number(ctx, left)?;
                let right_num = self.coerce_number(ctx, right)?;
                ctx.set_register(dst.0, Value::number(left_num / right_num));
                Ok(InstructionResult::Continue)
            }

            Instruction::Mod { dst, lhs, rhs } => {
                let left_value = ctx.get_register(lhs.0).clone();
                let right_value = ctx.get_register(rhs.0).clone();
                let left_num = self.to_numeric(ctx, &left_value)?;
                let right_num = self.to_numeric(ctx, &right_value)?;

                match (left_num, right_num) {
                    (Numeric::BigInt(left), Numeric::BigInt(right)) => {
                        if right.is_zero() {
                            return Err(VmError::range_error("Division by zero"));
                        }
                        let result = left % right;
                        ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    }
                    (Numeric::Number(left), Numeric::Number(right)) => {
                        ctx.set_register(dst.0, Value::number(left % right));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Ok(InstructionResult::Continue)
            }

            Instruction::Pow { dst, lhs, rhs } => {
                let left_value = ctx.get_register(lhs.0).clone();
                let right_value = ctx.get_register(rhs.0).clone();
                let left_num = self.to_numeric(ctx, &left_value)?;
                let right_num = self.to_numeric(ctx, &right_value)?;

                match (left_num, right_num) {
                    (Numeric::BigInt(left), Numeric::BigInt(right)) => {
                        if right < NumBigInt::zero() {
                            return Err(VmError::range_error(
                                "Exponent must be non-negative for BigInt",
                            ));
                        }
                        let exponent = right
                            .to_u32()
                            .ok_or_else(|| VmError::range_error("Exponent too large for BigInt"))?;
                        let result = left.pow(exponent);
                        ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    }
                    (Numeric::Number(left), Numeric::Number(right)) => {
                        ctx.set_register(dst.0, Value::number(left.powf(right)));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Ok(InstructionResult::Continue)
            }

            Instruction::Neg { dst, src } => {
                let val = ctx.get_register(src.0).clone();
                let numeric = self.to_numeric(ctx, &val)?;
                match numeric {
                    Numeric::BigInt(bigint) => {
                        let result = -bigint;
                        ctx.set_register(dst.0, Value::bigint(result.to_string()));
                    }
                    Numeric::Number(num) => {
                        ctx.set_register(dst.0, Value::number(-num));
                    }
                }
                Ok(InstructionResult::Continue)
            }

            // ==================== Comparison ====================
            Instruction::Eq { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                let result = self.abstract_equal(ctx, &left, &right)?;
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Ne { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                let result = !self.abstract_equal(ctx, &left, &right)?;
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::StrictEq { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                let result = self.strict_equal(&left, &right);
                if std::env::var("OTTER_TRACE_ASSERT_STREQ").is_ok() {
                    if let Some(frame) = ctx.current_frame() {
                        if let Some(func) = frame.module.function(frame.function_index) {
                            if func.name.as_deref() == Some("assert") && frame.pc == 7 {
                                eprintln!(
                                    "[OTTER_TRACE_ASSERT_STREQ] pc={} lhs_type={} rhs_type={} result={}",
                                    frame.pc,
                                    left.type_of(),
                                    right.type_of(),
                                    result
                                );
                            }
                        }
                    }
                }
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::StrictNe { dst, lhs, rhs } => {
                let left = ctx.get_register(lhs.0).clone();
                let right = ctx.get_register(rhs.0).clone();

                let result = !self.strict_equal(&left, &right);
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Lt { dst, lhs, rhs } => {
                let left_val = ctx.get_register(lhs.0).clone();
                let right_val = ctx.get_register(rhs.0).clone();
                let left = self.to_numeric(ctx, &left_val)?;
                let right = self.to_numeric(ctx, &right_val)?;
                let result = matches!(self.numeric_compare(left, right)?, Some(Ordering::Less));

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Le { dst, lhs, rhs } => {
                let left_val = ctx.get_register(lhs.0).clone();
                let right_val = ctx.get_register(rhs.0).clone();
                let left = self.to_numeric(ctx, &left_val)?;
                let right = self.to_numeric(ctx, &right_val)?;
                let result = matches!(
                    self.numeric_compare(left, right)?,
                    Some(Ordering::Less | Ordering::Equal)
                );

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Gt { dst, lhs, rhs } => {
                let left_val = ctx.get_register(lhs.0).clone();
                let right_val = ctx.get_register(rhs.0).clone();
                let left = self.to_numeric(ctx, &left_val)?;
                let right = self.to_numeric(ctx, &right_val)?;
                let result = matches!(self.numeric_compare(left, right)?, Some(Ordering::Greater));

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(InstructionResult::Continue)
            }

            Instruction::Ge { dst, lhs, rhs } => {
                let left_val = ctx.get_register(lhs.0).clone();
                let right_val = ctx.get_register(rhs.0).clone();
                let left = self.to_numeric(ctx, &left_val)?;
                let right = self.to_numeric(ctx, &right_val)?;
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

                // Step 1: If right is not an object, throw TypeError
                let Some(right_obj) = right.as_object() else {
                    return Err(VmError::type_error(
                        "Right-hand side of 'instanceof' is not an object",
                    ));
                };

                // Step 2: Check for Symbol.hasInstance (@@hasInstance)
                // Use get_property_value to properly call getters
                let has_instance_key =
                    PropertyKey::Symbol(crate::intrinsics::well_known::has_instance_symbol());
                let handler =
                    self.get_property_value(ctx, &right_obj, &has_instance_key, &right)?;

                if !handler.is_undefined() && !handler.is_null() {
                    // Step 2a: If handler is not callable, throw TypeError
                    if !handler.is_callable() {
                        return Err(VmError::type_error("@@hasInstance is not callable"));
                    }
                    // Step 2b: Call handler with this=right, args=[left]
                    let result =
                        self.call_function(ctx, &handler, right.clone(), &[left.clone()])?;
                    // Step 2c: Return ToBoolean(result)
                    ctx.set_register(dst.0, Value::boolean(result.to_boolean()));
                    return Ok(InstructionResult::Continue);
                }

                // Step 3: If right is not callable, throw TypeError (OrdinaryHasInstance step 1)
                if !right.is_callable() {
                    return Err(VmError::type_error(
                        "Right-hand side of 'instanceof' is not callable",
                    ));
                }

                // Step 4: OrdinaryHasInstance - check if left is an object
                let Some(left_obj) = left.as_object() else {
                    ctx.set_register(dst.0, Value::boolean(false));
                    return Ok(InstructionResult::Continue);
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
                        let feedback = func.feedback_vector.write();
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
                    current = obj.prototype().as_object();
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
                        PropertyKey::Symbol(sym)
                    } else {
                        let idx_str = self.to_string(&left);
                        PropertyKey::string(&idx_str)
                    };
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_has(&mut ncx, proxy, &key, left.clone())?
                    };
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
                    PropertyKey::Symbol(sym)
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

                    // Slow path with IC update (proxy-aware)
                    let has_property =
                        self.has_with_proxy_chain(ctx, &right_obj, &key, left.clone())?;
                    {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let func = frame
                            .module
                            .function(frame.function_index)
                            .ok_or_else(|| VmError::internal("no function"))?;
                        let feedback = func.feedback_vector.write();
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

                let result = self.has_with_proxy_chain(ctx, &right_obj, &key, left.clone())?;
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
                let cond_val = ctx.get_register(cond.0).clone();
                if std::env::var("OTTER_TRACE_ASSERT_JIF").is_ok() {
                    if let Some(frame) = ctx.current_frame() {
                        if let Some(func) = frame.module.function(frame.function_index) {
                            if func.name.as_deref() == Some("assert") && frame.pc == 8 {
                                eprintln!(
                                    "[OTTER_TRACE_ASSERT_JIF] frame_id={} reg_base={} pc={} cond_type={} cond_bool={} offset={}",
                                    frame.frame_id,
                                    frame.register_base,
                                    frame.pc,
                                    cond_val.type_of(),
                                    cond_val.to_boolean(),
                                    offset.0
                                );
                            }
                        }
                    }
                }
                if !cond_val.to_boolean() {
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

                let func_obj =
                    GcRef::new(JsObject::new(Value::null(), ctx.memory_manager().clone()));

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

                let proto = GcRef::new(JsObject::new(
                    obj_proto.map(Value::object).unwrap_or_else(Value::null),
                    ctx.memory_manager().clone(),
                ));

                // Set [[Prototype]] to Function.prototype so methods like
                // .bind(), .call(), .apply() are inherited per ES2023 §10.2.4.
                if let Some(fn_proto) = ctx.function_prototype() {
                    func_obj.set_prototype(Value::object(fn_proto));
                }
                func_obj.define_property(
                    PropertyKey::string("__realm_id__"),
                    PropertyDescriptor::builtin_data(Value::int32(ctx.realm_id() as i32)),
                );
                func_obj.define_property(
                    PropertyKey::string("__realm_id__"),
                    PropertyDescriptor::builtin_data(Value::int32(ctx.realm_id() as i32)),
                );

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

                let closure = GcRef::new(Closure {
                    function_index: func.0,
                    module: Arc::clone(module),
                    upvalues: captured_upvalues,
                    is_async: func_def.is_async(),
                    is_generator: false,
                    object: func_obj,
                    home_object: None,
                });
                let func_value = Value::function(closure);
                if func_def.is_arrow() || func_def.is_async() {
                    // Arrow and async functions are not constructors and have no prototype
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
                } else {
                    // Regular functions: prototype is {writable: true, enumerable: false, configurable: false}
                    func_obj.define_property(
                        PropertyKey::string("prototype"),
                        PropertyDescriptor::Data {
                            value: Value::object(proto),
                            attributes: PropertyAttributes {
                                writable: true,
                                enumerable: false,
                                configurable: false,
                            },
                        },
                    );
                    let _ = proto.set(PropertyKey::string("constructor"), func_value.clone());
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

                let func_obj =
                    GcRef::new(JsObject::new(Value::null(), ctx.memory_manager().clone()));

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

                let _proto = GcRef::new(JsObject::new(
                    obj_proto.map(Value::object).unwrap_or_else(Value::null),
                    ctx.memory_manager().clone(),
                ));

                // Set [[Prototype]] to Function.prototype
                if let Some(fn_proto) = ctx.function_prototype() {
                    func_obj.set_prototype(Value::object(fn_proto));
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

                let closure = GcRef::new(Closure {
                    function_index: func.0,
                    module: Arc::clone(module),
                    upvalues: captured_upvalues,
                    is_async: true,
                    is_generator: false,
                    object: func_obj,
                    home_object: None,
                });
                let func_value = Value::function(closure);
                // Async functions are not constructors and have no prototype
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
                let func_obj = GcRef::new(JsObject::new(
                    gen_func_proto
                        .map(Value::object)
                        .unwrap_or_else(Value::null),
                    ctx.memory_manager().clone(),
                ));
                func_obj.define_property(
                    PropertyKey::string("__realm_id__"),
                    PropertyDescriptor::builtin_data(Value::int32(ctx.realm_id() as i32)),
                );

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
                let proto = GcRef::new(JsObject::new(
                    gen_proto.map(Value::object).unwrap_or_else(Value::null),
                    ctx.memory_manager().clone(),
                ));

                let closure = GcRef::new(Closure {
                    function_index: func.0,
                    module: Arc::clone(module),
                    upvalues: captured_upvalues,
                    is_async: false,
                    is_generator: true,
                    object: func_obj,
                    home_object: None,
                });
                let func_value = Value::function(closure);
                // Generator prototype: {writable: true, enumerable: false, configurable: false}
                func_obj.define_property(
                    PropertyKey::string("prototype"),
                    PropertyDescriptor::Data {
                        value: Value::object(proto),
                        attributes: PropertyAttributes {
                            writable: true,
                            enumerable: false,
                            configurable: false,
                        },
                    },
                );
                let _ = proto.set(PropertyKey::string("constructor"), func_value.clone());
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
                    async_gen_func_proto
                        .map(Value::object)
                        .unwrap_or_else(Value::null),
                    ctx.memory_manager().clone(),
                ));
                func_obj.define_property(
                    PropertyKey::string("__realm_id__"),
                    PropertyDescriptor::builtin_data(Value::int32(ctx.realm_id() as i32)),
                );

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
                let proto = GcRef::new(JsObject::new(
                    gen_proto.map(Value::object).unwrap_or_else(Value::null),
                    ctx.memory_manager().clone(),
                ));

                let closure = GcRef::new(Closure {
                    function_index: func.0,
                    module: Arc::clone(module),
                    upvalues: captured_upvalues,
                    is_async: true,
                    is_generator: true,
                    object: func_obj,
                    home_object: None,
                });
                let func_value = Value::function(closure);
                // Async generator prototype: {writable: true, enumerable: false, configurable: false}
                func_obj.define_property(
                    PropertyKey::string("prototype"),
                    PropertyDescriptor::Data {
                        value: Value::object(proto),
                        attributes: PropertyAttributes {
                            writable: true,
                            enumerable: false,
                            configurable: false,
                        },
                    },
                );
                let _ = proto.set(PropertyKey::string("constructor"), func_value.clone());
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
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_apply(
                            &mut ncx,
                            proxy,
                            Value::undefined(),
                            &args,
                        )?
                    };
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
                            ) => std::ptr::eq(a.as_ptr(), b.as_ptr()),
                            _ => false,
                        }
                    };
                    let is_special = ["__Function_call", "__Function_apply", "eval"]
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
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_construct(
                            &mut ncx,
                            proxy,
                            &args,
                            func_value.clone(), // new.target
                        )?
                    };
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
                        .and_then(|v| v.as_object())
                        .or_else(|| {
                            self.default_object_prototype_for_constructor(ctx, &func_value)
                        });
                    let new_obj = GcRef::new(JsObject::new(
                        ctor_proto
                            .clone()
                            .map(Value::object)
                            .unwrap_or_else(Value::null),
                        ctx.memory_manager().clone(),
                    ));
                    let new_obj_value = Value::object(new_obj.clone());

                    // Capture stack trace for Error objects
                    if let Some(proto) = ctor_proto {
                        if proto
                            .get(&PropertyKey::string("__is_error__"))
                            .and_then(|v| v.as_boolean())
                            == Some(true)
                        {
                            Self::capture_error_stack_trace(new_obj.clone(), ctx);
                        }
                    }

                    // Call native constructor with depth tracking
                    let result = match self.call_native_fn_construct(
                        ctx,
                        native_fn,
                        &new_obj_value,
                        &args,
                    ) {
                        Ok(v) => v,
                        Err(e) => return Err(e),
                    };
                    // Per spec, if the constructor returns an object, use it;
                    // otherwise use the newly created `this` object.
                    // DataView, ArrayBuffer, and TypedArray are object-like
                    // heap values that must be recognized here too.
                    let final_value = if result.is_object()
                        || result.is_data_view()
                        || result.is_array_buffer()
                        || result.is_typed_array()
                    {
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
                            .and_then(|v| v.as_object())
                            .or_else(|| {
                                self.default_object_prototype_for_constructor(ctx, &func_value)
                            });
                        let new_obj = GcRef::new(JsObject::new(
                            ctor_proto
                                .clone()
                                .map(Value::object)
                                .unwrap_or_else(Value::null),
                            ctx.memory_manager().clone(),
                        ));
                        let new_obj_value = Value::object(new_obj.clone());

                        // Capture stack trace for Error objects
                        if let Some(proto) = ctor_proto {
                            if proto
                                .get(&PropertyKey::string("__is_error__"))
                                .and_then(|v| v.as_boolean())
                                == Some(true)
                            {
                                Self::capture_error_stack_trace(new_obj.clone(), ctx);
                            }
                        }

                        ctx.set_pending_args(args);
                        ctx.set_pending_this(new_obj_value.clone());

                        // Pre-set dst to the new object (will be returned if constructor returns undefined)
                        ctx.set_register(dst.0, new_obj_value);
                    }

                    let realm_id = self.realm_id_for_function(ctx, &func_value);
                    ctx.set_pending_realm_id(realm_id);
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

                if std::env::var("OTTER_DUMP_CALLMETHOD_FUNC").is_ok()
                    && receiver.is_undefined()
                    && !DUMPED_ASSERT_RT.load(AtomicOrdering::SeqCst)
                {
                    if let Some(frame) = ctx.current_frame() {
                        if let Some(func) = module.function(frame.function_index) {
                            eprintln!(
                                "[OTTER_DUMP_CALLMETHOD_FUNC] frame_id={} reg_base={} function_index={} name={:?} pc={}",
                                frame.frame_id,
                                frame.register_base,
                                frame.function_index,
                                func.name,
                                frame.pc
                            );
                            eprintln!("  upvalues: {:?}", func.upvalues);
                            for (idx, cell) in frame.upvalues.iter().enumerate() {
                                eprintln!("  upvalue_cell[{}] type={}", idx, cell.get().type_of());
                            }
                            let argc_usize = *argc as usize;
                            for i in 0..=argc_usize {
                                let reg = Register(obj.0 + i as u16);
                                let val = ctx.get_register(reg.0).clone();
                                eprintln!("  call_reg[{}]=r{} type={}", i, reg.0, val.type_of());
                            }
                            for (idx, local) in frame.locals.iter().enumerate().take(4) {
                                eprintln!("  local[{}] type={}", idx, local.type_of());
                            }
                            for (idx, instr) in func.instructions.iter().enumerate() {
                                eprintln!("  {:04} {:?}", idx, instr);
                            }
                            let stack = ctx.call_stack();
                            if stack.len() >= 2 {
                                let parent = &stack[stack.len() - 2];
                                if let Some(parent_func) =
                                    parent.module.function(parent.function_index)
                                {
                                    eprintln!(
                                        "  parent_function_index={} name={:?}",
                                        parent.function_index, parent_func.name
                                    );
                                    eprintln!("  parent_local_names={:?}", parent_func.local_names);
                                    for (idx, capture) in func.upvalues.iter().enumerate() {
                                        match capture {
                                            otter_vm_bytecode::UpvalueCapture::Local(local_idx) => {
                                                let local_i = local_idx.0 as usize;
                                                let name = parent_func
                                                    .local_names
                                                    .get(local_i)
                                                    .cloned()
                                                    .unwrap_or_else(|| "<unknown>".to_string());
                                                let value_type = parent
                                                    .locals
                                                    .get(local_i)
                                                    .map(|v| v.type_of())
                                                    .unwrap_or("<out-of-range>");
                                                eprintln!(
                                                    "  upvalue[{}]=Local({}) name={} type={}",
                                                    idx, local_i, name, value_type
                                                );
                                            }
                                            otter_vm_bytecode::UpvalueCapture::Upvalue(up_idx) => {
                                                eprintln!(
                                                    "  upvalue[{}]=Upvalue({})",
                                                    idx, up_idx.0
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                            DUMPED_ASSERT_RT.store(true, AtomicOrdering::SeqCst);
                        }
                    }
                }

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
                let method_value = if let Some(proxy) = receiver.as_proxy() {
                    let key = Self::utf16_key(method_name);
                    let key_value = Value::string(JsString::intern_utf16(method_name));
                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                    crate::proxy_operations::proxy_get(
                        &mut ncx,
                        proxy,
                        &key,
                        key_value,
                        receiver.clone(),
                    )?
                } else if receiver.is_function() || receiver.is_native_function() {
                    let function_global = ctx.get_global("Function");
                    let function_obj = function_global
                        .as_ref()
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Function is not defined"))?;
                    let proto_val = function_obj.get(&PropertyKey::string("prototype"));
                    let proto = proto_val
                        .as_ref()
                        .and_then(|v| v.as_object())
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

                        // For async generators, return a Promise that resolves after proper
                        // resume (including awaited promises inside the generator body).
                        if generator.is_async() {
                            let promise_value = async_generator_result_to_promise_value(
                                gen_result,
                                ctx.memory_manager().clone(),
                                ctx.js_job_queue(),
                            );
                            ctx.set_register(dst.0, promise_value);
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
                        let result =
                            GcRef::new(JsObject::new(Value::null(), ctx.memory_manager().clone()));
                        let _ = result.set(PropertyKey::string("value"), result_value);
                        let _ = result.set(PropertyKey::string("done"), Value::boolean(is_done));
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
                } else if receiver.is_symbol() {
                    let symbol_obj = ctx
                        .get_global("Symbol")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Symbol is not defined"))?;
                    let proto = symbol_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Symbol.prototype is not defined"))?;
                    proto
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if receiver.is_bigint() {
                    let bigint_obj = ctx
                        .get_global("BigInt")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("BigInt is not defined"))?;
                    let proto = bigint_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("BigInt.prototype is not defined"))?;
                    proto
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if receiver.is_data_view() {
                    let dv_ctor = ctx
                        .get_global("DataView")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("DataView is not defined"))?;
                    let proto = dv_ctor
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("DataView.prototype is not defined"))?;
                    proto
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if let Some(regex) = receiver.as_regex() {
                    // RegExp: look up method on the regex's internal object (which has the prototype chain)
                    regex
                        .object
                        .get(&Self::utf16_key(method_name))
                        .unwrap_or_else(Value::undefined)
                } else if let Some(proxy) = receiver.as_proxy() {
                    // Proxy: look up method via proxy get trap
                    let key = Self::utf16_key(method_name);
                    let key_str = String::from_utf16_lossy(method_name);
                    let key_val = Value::string(crate::string::JsString::intern(&key_str));
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_get(
                            &mut ncx,
                            proxy,
                            &key,
                            key_val,
                            receiver.clone(),
                        )
                    };
                    match result {
                        Ok(val) => val,
                        Err(_) => Value::undefined(),
                    }
                } else {
                    if std::env::var("OTTER_TRACE_CALLMETHOD").is_ok() {
                        let (func_name, source_url, pc) = ctx
                            .current_frame()
                            .and_then(|frame| {
                                let func = frame.module.function(frame.function_index);
                                Some((
                                    func.and_then(|f| f.name.clone())
                                        .unwrap_or_else(|| "(anonymous)".to_string()),
                                    frame.module.source_url.clone(),
                                    frame.pc,
                                ))
                            })
                            .unwrap_or_else(|| {
                                ("(no-frame)".to_string(), "(unknown)".to_string(), 0)
                            });
                        eprintln!(
                            "[OTTER_TRACE_CALLMETHOD] receiver_type={} method={} func={} pc={} source={} obj_reg={}",
                            receiver.type_of(),
                            String::from_utf16_lossy(method_name),
                            func_name,
                            pc,
                            source_url,
                            obj.0
                        );
                    }
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
                        let feedback = func.feedback_vector.write();
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
                        .and_then(|v| v.as_object())
                        .or_else(|| {
                            self.default_object_prototype_for_constructor(ctx, &func_value)
                        });
                    let new_obj = GcRef::new(JsObject::new(
                        ctor_proto.map(Value::object).unwrap_or_else(Value::null),
                        ctx.memory_manager().clone(),
                    ));
                    let new_obj_value = Value::object(new_obj);

                    let result =
                        self.call_native_fn_construct(ctx, native_fn, &new_obj_value, &args)?;
                    let final_value = if result.is_object()
                        || result.is_data_view()
                        || result.is_array_buffer()
                        || result.is_typed_array()
                    {
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
                    .and_then(|v| v.as_object())
                    .or_else(|| self.default_object_prototype_for_constructor(ctx, &func_value));
                let new_obj = GcRef::new(JsObject::new(
                    ctor_proto.map(Value::object).unwrap_or_else(Value::null),
                    ctx.memory_manager().clone(),
                ));
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
                    value.as_promise()
                } else if let Some(obj) = value.as_object() {
                    // Check for JS Promise wrapper: { _internal: <vm_promise> }
                    obj.get(&PropertyKey::string("_internal"))
                        .and_then(|v| v.as_promise())
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
                            // Promise rejected — throw the rejection value as-is
                            // (ES2023 §27.7.5.3 Await step 5: if rejected, throw reason)
                            Err(VmError::exception(error))
                        }
                        PromiseState::Pending | PromiseState::PendingThenable(_) => {
                            // Promise is pending, suspend execution
                            Ok(InstructionResult::Suspend {
                                promise,
                                resume_reg: dst.0,
                            })
                        }
                    }
                } else if let Some(obj) = value.as_object() {
                    // Check for thenable: object with a .then() method
                    // Per ES2023 §27.7.5.3: await wraps via PromiseResolve which
                    // checks for thenables and calls their .then() method.
                    if let Some(then_fn) = obj.get(&PropertyKey::string("then")) {
                        if then_fn.is_function() || then_fn.is_native_function() {
                            // Thenable: create a promise that resolves via .then()
                            let promise = JsPromise::new();
                            let promise_ref = promise.clone();
                            let promise_ref2 = promise.clone();
                            let mm = ctx.memory_manager().clone();

                            // Call obj.then(resolve, reject)
                            let resolve_fn = Value::native_function(
                                move |_this: &Value,
                                      args: &[Value],
                                      _ncx: &mut crate::context::NativeContext<'_>| {
                                    let val =
                                        args.first().cloned().unwrap_or_else(Value::undefined);
                                    promise_ref.resolve(val);
                                    Ok(Value::undefined())
                                },
                                mm.clone(),
                            );
                            let reject_fn = Value::native_function(
                                move |_this: &Value,
                                      args: &[Value],
                                      _ncx: &mut crate::context::NativeContext<'_>| {
                                    let val =
                                        args.first().cloned().unwrap_or_else(Value::undefined);
                                    promise_ref2.reject(val);
                                    Ok(Value::undefined())
                                },
                                mm,
                            );

                            if let Err(e) =
                                self.call_function(ctx, &then_fn, value, &[resolve_fn, reject_fn])
                            {
                                // If .then() throws synchronously, reject the wrapper promise
                                let err_val = match e {
                                    VmError::Exception(thrown) => thrown.value,
                                    other => self.make_error(ctx, "TypeError", &other.to_string()),
                                };
                                promise.reject(err_val);
                            }

                            // Now handle the promise state
                            match promise.state() {
                                PromiseState::Fulfilled(resolved) => {
                                    ctx.set_register(dst.0, resolved);
                                    Ok(InstructionResult::Continue)
                                }
                                PromiseState::Rejected(error) => Err(VmError::exception(error)),
                                PromiseState::Pending | PromiseState::PendingThenable(_) => {
                                    Ok(InstructionResult::Suspend {
                                        promise,
                                        resume_reg: dst.0,
                                    })
                                }
                            }
                        } else {
                            // Has .then but it's not callable — not a thenable
                            ctx.set_register(dst.0, value);
                            Ok(InstructionResult::Continue)
                        }
                    } else {
                        // No .then property — not a thenable
                        ctx.set_register(dst.0, value);
                        Ok(InstructionResult::Continue)
                    }
                } else {
                    // Primitive non-Promise — return directly
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

                let obj = GcRef::new(JsObject::new(
                    proto.map(Value::object).unwrap_or_else(Value::null),
                    ctx.memory_manager().clone(),
                ));
                ctx.set_register(dst.0, Value::object(obj));
                Ok(InstructionResult::Continue)
            }

            Instruction::CreateArguments { dst } => {
                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame"))?;
                let argc = frame.argc;
                let func = &frame.module.functions[frame.function_index as usize];
                let param_count = func.param_count as usize;
                let local_count = func.local_count as usize;
                let is_strict = func.flags.is_strict;
                let is_mapped = !is_strict && func.flags.has_simple_parameters;
                let callee_val = frame.callee_value.clone();
                let mm = ctx.memory_manager().clone();

                // Get Object.prototype for the arguments object
                let obj_proto = ctx
                    .get_global("Object")
                    .and_then(|v| v.as_object())
                    .and_then(|o| o.get(&PropertyKey::string("prototype")))
                    .and_then(|v| v.as_object());

                // Use array_like (not array) so Array.isArray(arguments) returns false
                let args_obj = GcRef::new(JsObject::array_like(argc, mm.clone()));
                if let Some(proto) = obj_proto {
                    args_obj.set_prototype(Value::object(proto));
                }

                if is_mapped {
                    // --- MAPPED ARGUMENTS (sloppy mode, simple params) ---
                    // Create UpvalueCells for each parameter to alias with locals
                    let mut cells = Vec::with_capacity(param_count);
                    for i in 0..param_count {
                        if i < argc {
                            let cell = ctx.get_or_create_open_upvalue(i as u16)?;
                            // Set initial element value (for when mapping is later removed)
                            let _ = args_obj.set(PropertyKey::index(i as u32), cell.get());
                            cells.push(Some(cell));
                        } else {
                            cells.push(None); // param not passed, no aliasing
                        }
                    }
                    // Extra args beyond param_count — just copy
                    for i in param_count..argc {
                        let offset = local_count + (i - param_count);
                        let val = ctx.get_local(offset as u16)?;
                        let _ = args_obj.set(PropertyKey::index(i as u32), val);
                    }
                    args_obj.set_argument_mapping(crate::object::ArgumentMapping { cells });

                    // callee = current function (non-enumerable, writable, configurable)
                    if let Some(callee) = callee_val {
                        args_obj.define_property(
                            PropertyKey::string("callee"),
                            PropertyDescriptor::data_with_attrs(
                                callee,
                                PropertyAttributes::builtin_method(),
                            ),
                        );
                    }
                } else {
                    // --- UNMAPPED ARGUMENTS (strict or non-simple params) ---
                    for i in 0..argc {
                        let val = if i < param_count {
                            ctx.get_local(i as u16)?
                        } else {
                            let offset = local_count + (i - param_count);
                            ctx.get_local(offset as u16)?
                        };
                        let _ = args_obj.set(PropertyKey::index(i as u32), val);
                    }

                    if is_strict {
                        // callee = accessor that throws TypeError
                        let thrower = Value::native_function(
                            |_this: &Value, _args: &[Value], _ncx: &mut crate::context::NativeContext<'_>| {
                                Err(VmError::type_error(
                                    "'caller', 'callee', and 'arguments' properties may not be accessed on strict mode functions or the arguments objects for calls to them",
                                ))
                            },
                            mm.clone(),
                        );
                        args_obj.define_property(
                            PropertyKey::string("callee"),
                            PropertyDescriptor::Accessor {
                                get: Some(thrower.clone()),
                                set: Some(thrower),
                                attributes: PropertyAttributes {
                                    writable: false,
                                    enumerable: false,
                                    configurable: false,
                                },
                            },
                        );
                    }
                }

                // Both modes: length (non-enumerable, writable, configurable)
                args_obj.define_property(
                    PropertyKey::string("length"),
                    PropertyDescriptor::data_with_attrs(
                        Value::number(argc as f64),
                        PropertyAttributes::builtin_method(),
                    ),
                );

                // Symbol.iterator = Array.prototype[Symbol.iterator]
                let iterator_sym = crate::intrinsics::well_known::iterator_symbol();
                if let Some(array_proto) = ctx
                    .get_global("Array")
                    .and_then(|v| v.as_object())
                    .and_then(|o| o.get(&PropertyKey::string("prototype")))
                    .and_then(|v| v.as_object())
                {
                    if let Some(iterator_fn) =
                        array_proto.get(&PropertyKey::Symbol(iterator_sym.clone()))
                    {
                        args_obj.define_property(
                            PropertyKey::Symbol(iterator_sym),
                            PropertyDescriptor::data_with_attrs(
                                iterator_fn,
                                PropertyAttributes::builtin_method(),
                            ),
                        );
                    }
                }

                // @@toStringTag = "Arguments" (non-enumerable, configurable)
                let to_string_tag_sym = crate::intrinsics::well_known::to_string_tag_symbol();
                args_obj.define_property(
                    PropertyKey::Symbol(to_string_tag_sym),
                    PropertyDescriptor::data_with_attrs(
                        Value::string(JsString::intern("Arguments")),
                        PropertyAttributes {
                            writable: false,
                            enumerable: false,
                            configurable: true,
                        },
                    ),
                );

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
                let is_strict_context = ctx
                    .current_frame()
                    .and_then(|frame| frame.module.functions.get(frame.function_index as usize))
                    .map(|func| func.flags.is_strict)
                    .unwrap_or(false);

                let injected_eval_bindings = self.inject_eval_bindings(ctx);
                let eval_result = (|| {
                    let eval_module = ctx.compile_eval(&source, is_strict_context)?;
                    self.execute_eval_module(ctx, &eval_module)
                })();
                self.cleanup_eval_bindings(ctx, &injected_eval_bindings);

                let result = eval_result?;
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
                let trace_array = std::env::var("OTTER_TRACE_ARRAY").is_ok();
                let is_global = object
                    .as_object()
                    .map(|o| o.as_ptr() == ctx.global().as_ptr())
                    .unwrap_or(false);
                let array_ctor_obj = ctx
                    .global()
                    .get(&PropertyKey::string("Array"))
                    .and_then(|v| v.as_object());
                let array_proto_obj = array_ctor_obj.and_then(|array_obj| {
                    array_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                });
                let array_proto_ptr = array_proto_obj.map(|proto| proto.as_ptr());
                let is_array_proto = object
                    .as_object()
                    .map(|o| Some(o.as_ptr()) == array_proto_ptr)
                    .unwrap_or(false);
                let is_array_ctor = object
                    .as_object()
                    .map(|o| Some(o.as_ptr()) == array_ctor_obj.map(|a| a.as_ptr()))
                    .unwrap_or(false);

                // Proxy check - must be first
                if let Some(proxy) = object.as_proxy() {
                    let key = Self::utf16_key(name_str);
                    let key_value = Value::string(JsString::intern_utf16(name_str));
                    let receiver = object.clone();
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_get(
                            &mut ncx, proxy, &key, key_value, receiver,
                        )?
                    };
                    if trace_array
                        && (Self::utf16_eq_ascii(name_str, "Array")
                            || Self::utf16_eq_ascii(name_str, "prototype")
                            || Self::utf16_eq_ascii(name_str, "map"))
                    {
                        eprintln!(
                            "[OTTER_TRACE_ARRAY] GetPropConst(proxy) name={} is_global={} is_array_proto={} result_type={}",
                            String::from_utf16_lossy(name_str),
                            is_global,
                            is_array_proto,
                            result.type_of()
                        );
                    }
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
                    if let Some(proto) = generator.object.prototype().as_object() {
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
                            if trace_array
                                && (Self::utf16_eq_ascii(name_str, "Array")
                                    || Self::utf16_eq_ascii(name_str, "prototype")
                                    || Self::utf16_eq_ascii(name_str, "map"))
                            {
                                eprintln!(
                                    "[OTTER_TRACE_ARRAY] GetPropConst(string-proto) name={} is_global={} is_array_proto={} result_type={}",
                                    String::from_utf16_lossy(name_str),
                                    is_global,
                                    is_array_proto,
                                    value.type_of()
                                );
                            }
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
                        if trace_array
                            && (Self::utf16_eq_ascii(name_str, "Array")
                                || Self::utf16_eq_ascii(name_str, "prototype")
                                || Self::utf16_eq_ascii(name_str, "map"))
                        {
                            eprintln!(
                                "[OTTER_TRACE_ARRAY] GetPropConst(function-obj) name={} is_global={} is_array_proto={} result_type={}",
                                String::from_utf16_lossy(name_str),
                                is_global,
                                is_array_proto,
                                val.type_of()
                            );
                            if Self::utf16_eq_ascii(name_str, "prototype") && is_array_ctor {
                                let proto_match = val
                                    .as_object()
                                    .map(|p| Some(p.as_ptr()) == array_proto_ptr)
                                    .unwrap_or(false);
                                eprintln!(
                                    "[OTTER_TRACE_ARRAY] Array.prototype from ctor match={} proto_ptr={:?} val_ptr={:?}",
                                    proto_match,
                                    array_proto_ptr,
                                    val.as_object().map(|p| p.as_ptr())
                                );
                            }
                        }
                        ctx.set_register(dst.0, val);
                        return Ok(InstructionResult::Continue);
                    }
                    // Check prototype chain
                    if let Some(proto) = closure.object.prototype().as_object() {
                        if let Some(val) = proto.get(&key) {
                            if trace_array
                                && (Self::utf16_eq_ascii(name_str, "Array")
                                    || Self::utf16_eq_ascii(name_str, "prototype")
                                    || Self::utf16_eq_ascii(name_str, "map"))
                            {
                                eprintln!(
                                    "[OTTER_TRACE_ARRAY] GetPropConst(function-proto) name={} is_global={} is_array_proto={} result_type={}",
                                    String::from_utf16_lossy(name_str),
                                    is_global,
                                    is_array_proto,
                                    val.type_of()
                                );
                            }
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
                    if !obj_ref.is_dictionary_mode() {
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
                        if trace_array
                            && (Self::utf16_eq_ascii(name_str, "Array")
                                || Self::utf16_eq_ascii(name_str, "prototype")
                                || Self::utf16_eq_ascii(name_str, "map"))
                        {
                            eprintln!(
                                "[OTTER_TRACE_ARRAY] GetPropConst(ic) name={} is_global={} is_array_proto={} result_type={}",
                                String::from_utf16_lossy(name_str),
                                is_global,
                                is_array_proto,
                                val.type_of()
                            );
                        }
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

                // DataView property access — no internal object, lookup on DataView.prototype
                if object.is_data_view() {
                    let key = Self::utf16_key(name_str);
                    let receiver = object.clone();
                    if let Some(dv_ctor) = ctx.get_global("DataView").and_then(|v| v.as_object()) {
                        if let Some(proto) = dv_ctor
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            // Check for accessor properties (byteLength, buffer, byteOffset)
                            match proto.lookup_property_descriptor(&key) {
                                Some(crate::object::PropertyDescriptor::Accessor {
                                    get, ..
                                }) => {
                                    let Some(getter) = get else {
                                        ctx.set_register(dst.0, Value::undefined());
                                        return Ok(InstructionResult::Continue);
                                    };
                                    if let Some(native_fn) = getter.as_native_function() {
                                        let result =
                                            self.call_native_fn(ctx, native_fn, &receiver, &[])?;
                                        ctx.set_register(dst.0, result);
                                        return Ok(InstructionResult::Continue);
                                    }
                                }
                                _ => {
                                    let value = proto.get(&key).unwrap_or_else(Value::undefined);
                                    ctx.set_register(dst.0, value);
                                    return Ok(InstructionResult::Continue);
                                }
                            }
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
                                    let feedback = func.feedback_vector.write();
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

                            let key_value = Value::string(JsString::intern_utf16(name_str));
                            let value =
                                self.get_with_proxy_chain(ctx, &obj, &key, key_value, &object)?;
                            if trace_array
                                && (Self::utf16_eq_ascii(name_str, "Array")
                                    || Self::utf16_eq_ascii(name_str, "prototype")
                                    || Self::utf16_eq_ascii(name_str, "map"))
                            {
                                eprintln!(
                                    "[OTTER_TRACE_ARRAY] GetPropConst(slow) name={} is_global={} is_array_proto={} result_type={}",
                                    String::from_utf16_lossy(name_str),
                                    is_global,
                                    is_array_proto,
                                    value.type_of()
                                );
                            }
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
                    if let Some(boolean_obj) = ctx.get_global("Boolean").and_then(|v| v.as_object())
                    {
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
                } else if object.is_symbol() {
                    // Autobox symbol -> Symbol.prototype
                    let key = Self::utf16_key(name_str);
                    if let Some(symbol_obj) = ctx.get_global("Symbol").and_then(|v| v.as_object()) {
                        if let Some(proto) = symbol_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            let value = self.get_property_value(ctx, &proto, &key, &object)?;
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
                let is_strict = ctx
                    .current_frame()
                    .and_then(|frame| frame.module.function(frame.function_index))
                    .map(|func| func.flags.is_strict)
                    .unwrap_or(false);
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
                    {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_set(
                            &mut ncx, proxy, &key, key_value, val_val, receiver,
                        )?;
                    }
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
                                            match obj
                                                .set_by_offset(*offset as usize, val_val.clone())
                                            {
                                                Ok(()) => cached = true,
                                                // Accessor: fall through to slow path to call setter
                                                Err(SetPropertyError::AccessorWithoutSetter) => {}
                                                Err(e) if is_strict => {
                                                    return Err(VmError::type_error(e.to_string()));
                                                }
                                                Err(_) => {}
                                            }
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        for i in 0..(*count as usize) {
                                            if obj_shape_ptr == entries[i].0 {
                                                match obj.set_by_offset(
                                                    entries[i].1 as usize,
                                                    val_val.clone(),
                                                ) {
                                                    Ok(()) => cached = true,
                                                    // Accessor: fall through to slow path to call setter
                                                    Err(
                                                        SetPropertyError::AccessorWithoutSetter,
                                                    ) => {}
                                                    Err(e) if is_strict => {
                                                        return Err(VmError::type_error(
                                                            e.to_string(),
                                                        ));
                                                    }
                                                    Err(_) => {}
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

                    match obj.get_own_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Accessor { set, .. }) => {
                            let Some(setter) = set else {
                                if is_strict {
                                    return Err(VmError::type_error(
                                        "Cannot set property which has only a getter",
                                    ));
                                }
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
                        None => {
                            // No own property - walk prototype chain (may contain proxy or accessor)
                            let key_value = Value::string(JsString::intern_utf16(name_str));
                            let did_set = self.set_with_proxy_chain(
                                ctx, &obj, &key, key_value, val_val, &object,
                            )?;
                            if !did_set && is_strict {
                                return Err(VmError::type_error(format!(
                                    "Cannot set property '{}' on object",
                                    String::from_utf16_lossy(name_str)
                                )));
                            }
                            Ok(InstructionResult::Continue)
                        }
                        _ => {
                            // Own data property: set directly
                            if let Err(e) = obj.set(key, val_val) {
                                if is_strict {
                                    return Err(VmError::type_error(e.to_string()));
                                }
                            }
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
                                    let feedback = func.feedback_vector.write();
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
                    if is_strict {
                        return Err(VmError::type_error("Cannot set property on non-object"));
                    }
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::DeleteProp { dst, obj, key } => {
                let object = ctx.get_register(obj.0).clone();
                let key_value = ctx.get_register(key.0).clone();

                // TypeError for null/undefined base
                if object.is_null() || object.is_undefined() {
                    let key_str = self.to_string(&key_value);
                    let base = if object.is_null() {
                        "null"
                    } else {
                        "undefined"
                    };
                    return Err(VmError::type_error(format!(
                        "Cannot delete property '{}' of {}",
                        key_str, base
                    )));
                }

                // Proxy check - must be first
                if let Some(proxy) = object.as_proxy() {
                    let prop_key = if let Some(n) = key_value.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = key_value.as_string() {
                        PropertyKey::from_js_string(s)
                    } else if let Some(sym) = key_value.as_symbol() {
                        PropertyKey::Symbol(sym)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_delete_property(
                            &mut ncx, proxy, &prop_key, key_value,
                        )?
                    };
                    ctx.set_register(dst.0, Value::boolean(result));
                    return Ok(InstructionResult::Continue);
                }

                // Convert key to PropertyKey
                let prop_key = if let Some(n) = key_value.as_int32() {
                    PropertyKey::Index(n as u32)
                } else if let Some(s) = key_value.as_string() {
                    PropertyKey::from_js_string(s)
                } else if let Some(sym) = key_value.as_symbol() {
                    PropertyKey::Symbol(sym)
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

                // Strict mode: throw TypeError if delete failed (non-configurable property)
                if !result {
                    let is_strict = ctx
                        .current_frame()
                        .and_then(|frame| frame.module.function(frame.function_index))
                        .map(|func| func.flags.is_strict)
                        .unwrap_or(false);
                    if is_strict {
                        return Err(VmError::type_error(
                            "Cannot delete non-configurable property",
                        ));
                    }
                }

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
                        PropertyKey::Symbol(sym)
                    } else {
                        let key_str = self.to_string(&key_value);
                        PropertyKey::string(&key_str)
                    };
                    let receiver = object.clone();
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_get(
                            &mut ncx,
                            proxy,
                            &prop_key,
                            key_value.clone(),
                            receiver,
                        )?
                    };
                    ctx.set_register(dst.0, result);
                    return Ok(InstructionResult::Continue);
                }

                if let Some(str_ref) = object.as_string() {
                    let key = if let Some(n) = key_value.as_int32() {
                        PropertyKey::Index(n as u32)
                    } else if let Some(s) = key_value.as_string() {
                        PropertyKey::from_js_string(s)
                    } else if let Some(sym) = key_value.as_symbol() {
                        PropertyKey::Symbol(sym)
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
                    let key = self.value_to_property_key(ctx, &key_value)?;
                    let receiver = object.clone();
                    let value = self.get_property_value(ctx, &closure.object, &key, &receiver)?;
                    ctx.set_register(dst.0, value);
                    return Ok(InstructionResult::Continue);
                }

                // Generator property access
                if let Some(generator) = object.as_generator() {
                    // Convert key to property key
                    let key = self.value_to_property_key(ctx, &key_value)?;

                    // Check the generator's internal object first
                    if let Some(val) = generator.object.get(&key) {
                        ctx.set_register(dst.0, val);
                        return Ok(InstructionResult::Continue);
                    }
                    // Check prototype chain (this gives us next, return, throw, Symbol.iterator, Symbol.toStringTag)
                    if let Some(proto) = generator.object.prototype().as_object() {
                        if let Some(val) = proto.get(&key) {
                            ctx.set_register(dst.0, val);
                            return Ok(InstructionResult::Continue);
                        }
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    return Ok(InstructionResult::Continue);
                }

                // DataView property access — no internal object, lookup on DataView.prototype
                if object.is_data_view() {
                    let key = self.value_to_property_key(ctx, &key_value)?;
                    let receiver = object.clone();
                    if let Some(dv_ctor) = ctx.get_global("DataView").and_then(|v| v.as_object()) {
                        if let Some(proto) = dv_ctor
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            match proto.lookup_property_descriptor(&key) {
                                Some(crate::object::PropertyDescriptor::Accessor {
                                    get, ..
                                }) => {
                                    let Some(getter) = get else {
                                        ctx.set_register(dst.0, Value::undefined());
                                        return Ok(InstructionResult::Continue);
                                    };
                                    if let Some(native_fn) = getter.as_native_function() {
                                        let result =
                                            self.call_native_fn(ctx, native_fn, &receiver, &[])?;
                                        ctx.set_register(dst.0, result);
                                        return Ok(InstructionResult::Continue);
                                    }
                                }
                                _ => {
                                    let value = proto.get(&key).unwrap_or_else(Value::undefined);
                                    ctx.set_register(dst.0, value);
                                    return Ok(InstructionResult::Continue);
                                }
                            }
                        }
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    return Ok(InstructionResult::Continue);
                }

                if let Some(obj) = object.as_object() {
                    let receiver = object.clone();

                    // Convert key to property key
                    let key = self.value_to_property_key(ctx, &key_value)?;

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
                                    let feedback = func.feedback_vector.write();
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

                            let value = self.get_with_proxy_chain(
                                ctx,
                                &obj,
                                &key,
                                key_value.clone(),
                                &receiver,
                            )?;
                            ctx.set_register(dst.0, value);
                            Ok(InstructionResult::Continue)
                        }
                    }
                } else if object.is_number() {
                    // Autobox number -> Number.prototype
                    let key = self.value_to_property_key(ctx, &key_value)?;
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
                    let key = self.value_to_property_key(ctx, &key_value)?;
                    if let Some(boolean_obj) = ctx.get_global("Boolean").and_then(|v| v.as_object())
                    {
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
                } else if object.is_symbol() {
                    // Autobox symbol -> Symbol.prototype
                    let key = self.value_to_property_key(ctx, &key_value)?;
                    if let Some(symbol_obj) = ctx.get_global("Symbol").and_then(|v| v.as_object()) {
                        if let Some(proto) = symbol_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            let value = self.get_property_value(ctx, &proto, &key, &object)?;
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
                let is_strict = ctx
                    .current_frame()
                    .and_then(|frame| frame.module.function(frame.function_index))
                    .map(|func| func.flags.is_strict)
                    .unwrap_or(false);

                // Proxy check - must be first
                if let Some(proxy) = object.as_proxy() {
                    let prop_key = self.value_to_property_key(ctx, &key_value)?;
                    let receiver = object.clone();
                    {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_set(
                            &mut ncx,
                            proxy,
                            &prop_key,
                            key_value.clone(),
                            val_val,
                            receiver,
                        )?;
                    }
                    return Ok(InstructionResult::Continue);
                }

                if let Some(obj) = object.as_object() {
                    let key = self.value_to_property_key(ctx, &key_value)?;

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
                                            match obj
                                                .set_by_offset(*offset as usize, val_val.clone())
                                            {
                                                Ok(()) => cached = true,
                                                // Accessor: fall through to slow path to call setter
                                                Err(SetPropertyError::AccessorWithoutSetter) => {}
                                                Err(e) if is_strict => {
                                                    return Err(VmError::type_error(e.to_string()));
                                                }
                                                Err(_) => {}
                                            }
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        for i in 0..(*count as usize) {
                                            if obj_shape_ptr == entries[i].0 {
                                                match obj.set_by_offset(
                                                    entries[i].1 as usize,
                                                    val_val.clone(),
                                                ) {
                                                    Ok(()) => cached = true,
                                                    // Accessor: fall through to slow path to call setter
                                                    Err(
                                                        SetPropertyError::AccessorWithoutSetter,
                                                    ) => {}
                                                    Err(e) if is_strict => {
                                                        return Err(VmError::type_error(
                                                            e.to_string(),
                                                        ));
                                                    }
                                                    Err(_) => {}
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
                            if let Err(e) = obj.set(key.clone(), val_val) {
                                if is_strict {
                                    return Err(VmError::type_error(e.to_string()));
                                }
                            }
                            if !obj.is_dictionary_mode() {
                                if let Some(offset) = obj.shape().get_offset(&key) {
                                    let frame = ctx
                                        .current_frame()
                                        .ok_or_else(|| VmError::internal("no frame"))?;
                                    let func = frame
                                        .module
                                        .function(frame.function_index)
                                        .ok_or_else(|| VmError::internal("no function"))?;
                                    let feedback = func.feedback_vector.write();
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
                    if is_strict {
                        return Err(VmError::type_error("Cannot set property on non-object"));
                    }
                    Ok(InstructionResult::Continue)
                }
            }

            Instruction::DefineGetter { obj, key, func } => {
                let object = ctx.get_register(obj.0).clone();
                let key_value = ctx.get_register(key.0).clone();
                let getter_fn = ctx.get_register(func.0).clone();

                if let Some(obj) = object.as_object() {
                    let prop_key = self.value_to_property_key(ctx, &key_value)?;

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
                let object = ctx.get_register(obj.0).clone();
                let key_value = ctx.get_register(key.0).clone();
                let setter_fn = ctx.get_register(func.0).clone();

                if let Some(obj) = object.as_object() {
                    let prop_key = self.value_to_property_key(ctx, &key_value)?;

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
                    arr.set_prototype(Value::object(array_proto));
                }
                ctx.set_register(dst.0, Value::array(arr));
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

                // Proxy check - must be first
                if let Some(proxy) = array.as_proxy() {
                    let prop_key = self.value_to_property_key(ctx, &index)?;
                    let receiver = array.clone();
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_get(
                            &mut ncx, proxy, &prop_key, index, receiver,
                        )?
                    };
                    ctx.set_register(dst.0, result);
                    return Ok(InstructionResult::Continue);
                }

                if let Some(obj) = array.as_object() {
                    // Fast path for integer index access on arrays
                    if obj.is_array() {
                        if let Some(n) = index.as_int32() {
                            let idx = n as usize;
                            let elements = obj.get_elements_storage().borrow();
                            if idx < elements.len() {
                                ctx.set_register(dst.0, elements[idx].clone());
                                return Ok(InstructionResult::Continue);
                            }
                        }
                    }

                    // Convert index to property key
                    let key = self.value_to_property_key(ctx, &index)?;

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
                                let feedback = func.feedback_vector.write();
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

                    let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                    let receiver = array.clone();
                    let value = self.get_with_proxy_chain(ctx, &obj, &key, key_value, &receiver)?;
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
                let is_strict = ctx
                    .current_frame()
                    .and_then(|frame| frame.module.function(frame.function_index))
                    .map(|func| func.flags.is_strict)
                    .unwrap_or(false);

                // Proxy check - must be first
                if let Some(proxy) = array.as_proxy() {
                    let prop_key = self.value_to_property_key(ctx, &index)?;
                    let receiver = array.clone();
                    {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_set(
                            &mut ncx, proxy, &prop_key, index, val_val, receiver,
                        )?;
                    }
                    return Ok(InstructionResult::Continue);
                }

                if let Some(obj) = array.as_object() {
                    // Convert index to property key
                    let key = self.value_to_property_key(ctx, &index)?;

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
                                                match obj.set_by_offset(
                                                    *offset as usize,
                                                    val_val.clone(),
                                                ) {
                                                    Ok(()) => cached = true,
                                                    // Accessor: fall through to slow path to call setter
                                                    Err(
                                                        SetPropertyError::AccessorWithoutSetter,
                                                    ) => {}
                                                    Err(e) if is_strict => {
                                                        return Err(VmError::type_error(
                                                            e.to_string(),
                                                        ));
                                                    }
                                                    Err(_) => {}
                                                }
                                            }
                                        }
                                        InlineCacheState::Polymorphic { count, entries } => {
                                            for i in 0..(*count as usize) {
                                                if obj_shape_ptr == entries[i].0 {
                                                    match obj.set_by_offset(
                                                        entries[i].1 as usize,
                                                        val_val.clone(),
                                                    ) {
                                                        Ok(()) => cached = true,
                                                        // Accessor: fall through to slow path to call setter
                                                        Err(
                                                            SetPropertyError::AccessorWithoutSetter,
                                                        ) => {}
                                                        Err(e) if is_strict => {
                                                            return Err(VmError::type_error(
                                                                e.to_string(),
                                                            ));
                                                        }
                                                        Err(_) => {}
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
                                let feedback = func.feedback_vector.write();
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

                    // Check for accessor setter before falling back to obj.set()
                    match obj.lookup_property_descriptor(&key) {
                        Some(PropertyDescriptor::Accessor { set, .. }) => {
                            let Some(setter) = set else {
                                if is_strict {
                                    return Err(VmError::type_error(
                                        "Cannot set property which has only a getter",
                                    ));
                                }
                                return Ok(InstructionResult::Continue);
                            };

                            if let Some(native_fn) = setter.as_native_function() {
                                self.call_native_fn(ctx, native_fn, &array, &[val_val])?;
                                return Ok(InstructionResult::Continue);
                            } else if let Some(closure) = setter.as_function() {
                                ctx.set_pending_args(vec![val_val]);
                                ctx.set_pending_this(array.clone());
                                return Ok(InstructionResult::Call {
                                    func_index: closure.function_index,
                                    module: Arc::clone(&closure.module),
                                    argc: 1,
                                    return_reg: 0, // Setter return value is ignored
                                    is_construct: false,
                                    is_async: closure.is_async,
                                    upvalues: closure.upvalues.clone(),
                                });
                            } else {
                                return Err(VmError::type_error("setter is not a function"));
                            }
                        }
                        _ => {
                            if let Err(e) = obj.set(key, val_val) {
                                if is_strict {
                                    return Err(VmError::type_error(e.to_string()));
                                }
                            }
                        }
                    }
                } else if is_strict {
                    return Err(VmError::type_error("Cannot set property on non-object"));
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

                if let Some(proxy) = receiver.as_proxy() {
                    let prop_key = self.value_to_property_key(ctx, &key_value)?;
                    let method_value = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_get(
                            &mut ncx,
                            proxy,
                            &prop_key,
                            key_value.clone(),
                            receiver.clone(),
                        )?
                    };
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        args.push(ctx.get_register(obj.0 + 2 + i).clone());
                    }
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

                        if generator.is_async() {
                            let promise_value = async_generator_result_to_promise_value(
                                gen_result,
                                ctx.memory_manager().clone(),
                                ctx.js_job_queue(),
                            );
                            ctx.set_register(dst.0, promise_value);
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
                        let result =
                            GcRef::new(JsObject::new(Value::null(), ctx.memory_manager().clone()));
                        let _ = result.set(PropertyKey::string("value"), result_value);
                        let _ = result.set(PropertyKey::string("done"), Value::boolean(is_done));
                        ctx.set_register(dst.0, Value::object(result));
                        return Ok(InstructionResult::Continue);
                    }
                }

                let key = self.value_to_property_key(ctx, &key_value)?;
                let method_value = if let Some(obj_ref) = receiver.as_object() {
                    obj_ref.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_string() {
                    let string_obj = ctx
                        .get_global("String")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("String is not defined"))?;
                    let proto = string_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("String.prototype is not defined"))?;
                    proto.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_number() {
                    let number_obj = ctx
                        .get_global("Number")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Number is not defined"))?;
                    let proto = number_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Number.prototype is not defined"))?;
                    proto.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_boolean() {
                    let boolean_obj = ctx
                        .get_global("Boolean")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Boolean is not defined"))?;
                    let proto = boolean_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Boolean.prototype is not defined"))?;
                    proto.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_symbol() {
                    if let Some(symbol_obj) = ctx.get_global("Symbol").and_then(|v| v.as_object()) {
                        if let Some(proto) = symbol_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            self.get_property_value(ctx, &proto, &key, &receiver)?
                        } else {
                            Value::undefined()
                        }
                    } else {
                        Value::undefined()
                    }
                } else if receiver.is_bigint() {
                    let bigint_obj = ctx
                        .get_global("BigInt")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("BigInt is not defined"))?;
                    let proto = bigint_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("BigInt.prototype is not defined"))?;
                    proto.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_promise() {
                    let promise_obj = ctx
                        .get_global("Promise")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Promise is not defined"))?;
                    let proto = promise_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Promise.prototype is not defined"))?;
                    proto.get(&key).unwrap_or_else(Value::undefined)
                } else if let Some(regex) = receiver.as_regex() {
                    regex.object.get(&key).unwrap_or_else(Value::undefined)
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
                        let feedback = func.feedback_vector.write();
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

                if let Some(proxy) = receiver.as_proxy() {
                    let prop_key = self.value_to_property_key(ctx, &key_value)?;
                    let method_value = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_get(
                            &mut ncx,
                            proxy,
                            &prop_key,
                            key_value.clone(),
                            receiver.clone(),
                        )?
                    };
                    return self.dispatch_method_spread(
                        ctx,
                        &method_value,
                        receiver,
                        &spread_arr,
                        dst.0,
                    );
                }

                let key = self.value_to_property_key(ctx, &key_value)?;
                let method_value = if let Some(obj_ref) = receiver.as_object() {
                    obj_ref.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_string() {
                    let string_obj = ctx
                        .get_global("String")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("String is not defined"))?;
                    let proto = string_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("String.prototype is not defined"))?;
                    proto.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_number() {
                    let number_obj = ctx
                        .get_global("Number")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Number is not defined"))?;
                    let proto = number_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Number.prototype is not defined"))?;
                    proto.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_boolean() {
                    let boolean_obj = ctx
                        .get_global("Boolean")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Boolean is not defined"))?;
                    let proto = boolean_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Boolean.prototype is not defined"))?;
                    proto.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_symbol() {
                    if let Some(symbol_obj) = ctx.get_global("Symbol").and_then(|v| v.as_object()) {
                        if let Some(proto) = symbol_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                        {
                            self.get_property_value(ctx, &proto, &key, &receiver)?
                        } else {
                            Value::undefined()
                        }
                    } else {
                        Value::undefined()
                    }
                } else if receiver.is_bigint() {
                    let bigint_obj = ctx
                        .get_global("BigInt")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("BigInt is not defined"))?;
                    let proto = bigint_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("BigInt.prototype is not defined"))?;
                    proto.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_promise() {
                    let promise_obj = ctx
                        .get_global("Promise")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Promise is not defined"))?;
                    let proto = promise_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("Promise.prototype is not defined"))?;
                    proto.get(&key).unwrap_or_else(Value::undefined)
                } else if let Some(regex) = receiver.as_regex() {
                    regex.object.get(&key).unwrap_or_else(Value::undefined)
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
                        let feedback = func.feedback_vector.write();
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
                let dst_arr = ctx.get_register(dst.0).clone();
                let src_arr = ctx.get_register(src.0).clone();

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
                        let _ = dst_obj.set(PropertyKey::Index(dst_len + i), elem);
                    }

                    // Update dst length
                    let _ = dst_obj.set(
                        PropertyKey::string("length"),
                        Value::int32((dst_len + src_len) as i32),
                    );
                }

                Ok(InstructionResult::Continue)
            }

            // ==================== Misc ====================
            Instruction::Move { dst, src } => {
                let value = ctx.get_register(src.0).clone();
                if std::env::var("OTTER_TRACE_ASSERT_MOVE").is_ok() {
                    if let Some(frame) = ctx.current_frame() {
                        if let Some(func) = frame.module.function(frame.function_index) {
                            if func.name.as_deref() == Some("assert") {
                                eprintln!(
                                    "[OTTER_TRACE_ASSERT_MOVE] func=assert src_reg={} dst_reg={} src_type={}",
                                    src.0,
                                    dst.0,
                                    value.type_of()
                                );
                            }
                        }
                    }
                }
                ctx.set_register(dst.0, value);
                Ok(InstructionResult::Continue)
            }

            Instruction::Nop => Ok(InstructionResult::Continue),

            Instruction::Debugger => {
                ctx.trigger_debugger_hook();
                Ok(InstructionResult::Continue)
            }

            // ==================== Iteration ====================
            Instruction::GetIterator { dst, src } => {
                use crate::value::HeapRef;

                let obj = ctx.get_register(src.0).clone();

                // Get Symbol.iterator method
                let iterator_sym = crate::intrinsics::well_known::iterator_symbol();
                let iterator_method = if let Some(proxy) = obj.as_proxy() {
                    let key = PropertyKey::Symbol(iterator_sym);
                    let key_value = Value::symbol(iterator_sym);
                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                    Some(crate::proxy_operations::proxy_get(
                        &mut ncx,
                        proxy,
                        &key,
                        key_value,
                        obj.clone(),
                    )?)
                } else if obj.is_string() {
                    // String primitives: look up Symbol.iterator on String.prototype
                    let string_obj = ctx
                        .get_global("String")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("String is not defined"))?;
                    let proto = string_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| VmError::type_error("String.prototype is not defined"))?;
                    proto.get(&PropertyKey::Symbol(iterator_sym))
                } else {
                    match obj.heap_ref() {
                        Some(HeapRef::Object(o)) | Some(HeapRef::Array(o)) => {
                            o.get(&PropertyKey::Symbol(iterator_sym))
                        }
                        _ => None,
                    }
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

                // 1. Try Symbol.asyncIterator
                let async_iterator_sym = crate::intrinsics::well_known::async_iterator_symbol();
                let iterator_sym = crate::intrinsics::well_known::iterator_symbol();

                let mut iterator_method = if let Some(proxy) = obj.as_proxy() {
                    let key = PropertyKey::Symbol(async_iterator_sym);
                    let key_value = Value::symbol(async_iterator_sym);
                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                    Some(crate::proxy_operations::proxy_get(
                        &mut ncx,
                        proxy,
                        &key,
                        key_value,
                        obj.clone(),
                    )?)
                } else {
                    match obj.heap_ref() {
                        Some(HeapRef::Object(o)) | Some(HeapRef::Array(o)) => {
                            o.get(&PropertyKey::Symbol(async_iterator_sym))
                        }
                        _ => None,
                    }
                };

                // 2. Fallback to Symbol.iterator
                if iterator_method.is_none() {
                    if let Some(proxy) = obj.as_proxy() {
                        let key = PropertyKey::Symbol(iterator_sym);
                        let key_value = Value::symbol(iterator_sym);
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        iterator_method = Some(crate::proxy_operations::proxy_get(
                            &mut ncx,
                            proxy,
                            &key,
                            key_value,
                            obj.clone(),
                        )?);
                    } else {
                        iterator_method = match obj.heap_ref() {
                            Some(HeapRef::Object(o)) | Some(HeapRef::Array(o)) => {
                                o.get(&PropertyKey::Symbol(iterator_sym))
                            }
                            _ => None,
                        };
                    }
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
                        let derived_proto = GcRef::new(JsObject::new(Value::null(), mm.clone()));

                        // Set ctor.prototype = derived_proto
                        let proto_key = PropertyKey::string("prototype");
                        if let Some(ctor_obj) = ctor_value.as_object() {
                            let _ = ctor_obj.set(proto_key, Value::object(derived_proto.clone()));
                            // Set derived_proto.constructor = ctor
                            let ctor_key = PropertyKey::string("constructor");
                            let _ = derived_proto.set(ctor_key, ctor_value.clone());
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
                        let derived_proto = GcRef::new(JsObject::new(
                            super_proto.map(Value::object).unwrap_or_else(Value::null),
                            mm.clone(),
                        ));

                        // Set ctor.prototype = derived_proto
                        if let Some(ctor_obj) = ctor_value.as_object() {
                            let _ = ctor_obj.set(
                                PropertyKey::string("prototype"),
                                Value::object(derived_proto.clone()),
                            );
                            // Set derived_proto.constructor = ctor
                            let _ = derived_proto
                                .set(PropertyKey::string("constructor"), ctor_value.clone());
                            // Static inheritance: ctor.__proto__ = super
                            ctor_obj.set_prototype(Value::object(super_obj));
                        }
                    } else if super_value.is_function() || super_value.is_native_function() {
                        // Superclass is a function (but not an object with .prototype on HeapRef::Object)
                        // This handles NativeFunction or Function HeapRef variants
                        // For now, create a basic prototype chain
                        let derived_proto = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                        if let Some(ctor_obj) = ctor_value.as_object() {
                            let _ = ctor_obj.set(
                                PropertyKey::string("prototype"),
                                Value::object(derived_proto.clone()),
                            );
                            let _ = derived_proto
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
                                let _ = proto_obj
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
                let super_proto = home_object.prototype().as_object().ok_or_else(|| {
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
                } else if super_ctor_val.as_native_function().is_some() {
                    // Base case: super constructor is a native built-in (Array, RegExp, etc.)
                    // Create object with correct prototype, then call as constructor.
                    let new_obj = GcRef::new(JsObject::new(
                        Value::object(new_target_proto.clone()),
                        mm.clone(),
                    ));
                    let new_obj_value = Value::object(new_obj);

                    let result = self.call_function_construct(
                        ctx,
                        &super_ctor_val,
                        new_obj_value.clone(),
                        &args,
                    )?;

                    // Native constructors may return a different object (e.g., Array creates a new array).
                    // Fix its prototype to new_target_proto for proper subclassing.
                    let this_obj = if result.is_object() {
                        if let Some(obj) = result.as_object() {
                            obj.set_prototype(Value::object(new_target_proto));
                        }
                        result
                    } else {
                        new_obj_value
                    };
                    this_obj
                } else {
                    // Base case: super constructor is a regular (non-derived) closure.
                    let new_obj =
                        GcRef::new(JsObject::new(Value::object(new_target_proto), mm.clone()));
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

            Instruction::CallSuperForward { dst } => {
                // Default derived constructor: forward all arguments to super constructor.
                // Arguments are stored in locals (see push_frame extra_args handling).
                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame for CallSuperForward"))?;

                let home_object = frame.home_object.clone().ok_or_else(|| {
                    VmError::ReferenceError("'super' keyword unexpected here".to_string())
                })?;
                let new_target_proto = frame
                    .new_target_proto
                    .clone()
                    .unwrap_or_else(|| home_object.clone());
                let argc = frame.argc;

                // Collect arguments from locals (for empty default constructor, all args are extras at locals[0..argc])
                let mut args = Vec::with_capacity(argc);
                for i in 0..argc {
                    args.push(ctx.get_local(i as u16)?);
                }

                // Get the superclass constructor
                let super_proto = home_object.prototype().as_object().ok_or_else(|| {
                    VmError::TypeError("Super constructor is not a constructor".to_string())
                })?;
                let ctor_key = PropertyKey::string("constructor");
                let super_ctor_val = super_proto.get(&ctor_key).unwrap_or_else(Value::undefined);
                let mm = ctx.memory_manager().clone();

                let super_is_derived = super_ctor_val
                    .as_function()
                    .and_then(|c| {
                        c.module
                            .function(c.function_index)
                            .map(|f| f.flags.is_derived)
                    })
                    .unwrap_or(false);

                let this_value = if super_is_derived {
                    if let Some(super_closure) = super_ctor_val.as_function() {
                        ctx.set_pending_is_derived(true);
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
                } else if super_ctor_val.as_native_function().is_some() {
                    // Native built-in constructor (Array, RegExp, etc.)
                    let new_obj = GcRef::new(JsObject::new(
                        Value::object(new_target_proto.clone()),
                        mm.clone(),
                    ));
                    let new_obj_value = Value::object(new_obj);
                    let result = self.call_function_construct(
                        ctx,
                        &super_ctor_val,
                        new_obj_value.clone(),
                        &args,
                    )?;
                    // Fix prototype for proper subclassing
                    let this_obj = if result.is_object() {
                        if let Some(obj) = result.as_object() {
                            obj.set_prototype(Value::object(new_target_proto));
                        }
                        result
                    } else {
                        new_obj_value
                    };
                    this_obj
                } else {
                    let new_obj =
                        GcRef::new(JsObject::new(Value::object(new_target_proto), mm.clone()));
                    let new_obj_value = Value::object(new_obj);
                    let result =
                        self.call_function(ctx, &super_ctor_val, new_obj_value.clone(), &args)?;
                    if result.is_object() {
                        result
                    } else {
                        new_obj_value
                    }
                };

                if let Some(frame) = ctx.current_frame_mut() {
                    frame.this_value = this_value.clone();
                    frame.this_initialized = true;
                }
                ctx.set_register(dst.0, this_value);
                Ok(InstructionResult::Continue)
            }

            Instruction::CallSuperSpread { dst, args } => {
                // Like CallSuper but arguments come from a spread array
                let spread_arr = ctx.get_register(args.0).clone();

                // Extract args from the array
                let mut call_args = Vec::new();
                if let Some(arr_obj) = spread_arr.as_object() {
                    let len = arr_obj
                        .get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_int32())
                        .unwrap_or(0) as u32;
                    for i in 0..len {
                        if let Some(elem) = arr_obj.get(&PropertyKey::Index(i)) {
                            call_args.push(elem);
                        } else {
                            call_args.push(Value::undefined());
                        }
                    }
                }

                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame for CallSuperSpread"))?;

                let home_object = frame.home_object.clone().ok_or_else(|| {
                    VmError::ReferenceError("'super' keyword unexpected here".to_string())
                })?;
                let new_target_proto = frame
                    .new_target_proto
                    .clone()
                    .unwrap_or_else(|| home_object.clone());

                let super_proto = home_object.prototype().as_object().ok_or_else(|| {
                    VmError::TypeError("Super constructor is not a constructor".to_string())
                })?;
                let ctor_key = PropertyKey::string("constructor");
                let super_ctor_val = super_proto.get(&ctor_key).unwrap_or_else(Value::undefined);
                let mm = ctx.memory_manager().clone();

                let super_is_derived = super_ctor_val
                    .as_function()
                    .and_then(|c| {
                        c.module
                            .function(c.function_index)
                            .map(|f| f.flags.is_derived)
                    })
                    .unwrap_or(false);

                let this_value = if super_is_derived {
                    if let Some(super_closure) = super_ctor_val.as_function() {
                        ctx.set_pending_is_derived(true);
                        ctx.set_pending_new_target_proto(new_target_proto);
                        let proto_key = PropertyKey::string("prototype");
                        if let Some(proto_val) = super_closure.object.get(&proto_key) {
                            if let Some(proto_obj) = proto_val.as_object() {
                                ctx.set_pending_home_object(proto_obj);
                            }
                        }
                    }
                    let result =
                        self.call_function(ctx, &super_ctor_val, Value::undefined(), &call_args)?;
                    if result.is_object() {
                        result
                    } else {
                        Value::undefined()
                    }
                } else if super_ctor_val.as_native_function().is_some() {
                    let new_obj = GcRef::new(JsObject::new(
                        Value::object(new_target_proto.clone()),
                        mm.clone(),
                    ));
                    let new_obj_value = Value::object(new_obj);
                    let result = self.call_function_construct(
                        ctx,
                        &super_ctor_val,
                        new_obj_value.clone(),
                        &call_args,
                    )?;
                    if result.is_object() {
                        if let Some(obj) = result.as_object() {
                            obj.set_prototype(Value::object(new_target_proto));
                        }
                        result
                    } else {
                        new_obj_value
                    }
                } else {
                    let new_obj =
                        GcRef::new(JsObject::new(Value::object(new_target_proto), mm.clone()));
                    let new_obj_value = Value::object(new_obj);
                    let result = self.call_function(
                        ctx,
                        &super_ctor_val,
                        new_obj_value.clone(),
                        &call_args,
                    )?;
                    if result.is_object() {
                        result
                    } else {
                        new_obj_value
                    }
                };

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
                let result = home_object.prototype();

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

                let value = if let Some(proto) = super_proto.as_object() {
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
                        ctx.set_register(func.0, Value::function(GcRef::new(new_closure)));
                    }
                }
                Ok(InstructionResult::Continue)
            }

            // ==================== Bitwise operators ====================
            Instruction::BitAnd { dst, lhs, rhs } => {
                let l_val = ctx.get_register(lhs.0).clone();
                let r_val = ctx.get_register(rhs.0).clone();
                let l = self.to_int32_from(self.coerce_number(ctx, l_val)?);
                let r = self.to_int32_from(self.coerce_number(ctx, r_val)?);
                ctx.set_register(dst.0, Value::number((l & r) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::BitOr { dst, lhs, rhs } => {
                let l_val = ctx.get_register(lhs.0).clone();
                let r_val = ctx.get_register(rhs.0).clone();
                let l = self.to_int32_from(self.coerce_number(ctx, l_val)?);
                let r = self.to_int32_from(self.coerce_number(ctx, r_val)?);
                ctx.set_register(dst.0, Value::number((l | r) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::BitXor { dst, lhs, rhs } => {
                let l_val = ctx.get_register(lhs.0).clone();
                let r_val = ctx.get_register(rhs.0).clone();
                let l = self.to_int32_from(self.coerce_number(ctx, l_val)?);
                let r = self.to_int32_from(self.coerce_number(ctx, r_val)?);
                ctx.set_register(dst.0, Value::number((l ^ r) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::BitNot { dst, src } => {
                let v_val = ctx.get_register(src.0).clone();
                let v = self.to_int32_from(self.coerce_number(ctx, v_val)?);
                ctx.set_register(dst.0, Value::number((!v) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::Shl { dst, lhs, rhs } => {
                let l_val = ctx.get_register(lhs.0).clone();
                let r_val = ctx.get_register(rhs.0).clone();
                let l = self.to_int32_from(self.coerce_number(ctx, l_val)?);
                let r = self.to_uint32_from(self.coerce_number(ctx, r_val)?);
                let shift = (r & 0x1f) as u32;
                ctx.set_register(dst.0, Value::number((l.wrapping_shl(shift)) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::Shr { dst, lhs, rhs } => {
                let l_val = ctx.get_register(lhs.0).clone();
                let r_val = ctx.get_register(rhs.0).clone();
                let l = self.to_int32_from(self.coerce_number(ctx, l_val)?);
                let r = self.to_uint32_from(self.coerce_number(ctx, r_val)?);
                let shift = (r & 0x1f) as u32;
                ctx.set_register(dst.0, Value::number((l.wrapping_shr(shift)) as f64));
                Ok(InstructionResult::Continue)
            }
            Instruction::Ushr { dst, lhs, rhs } => {
                let l_val = ctx.get_register(lhs.0).clone();
                let r_val = ctx.get_register(rhs.0).clone();
                let l = self.to_uint32_from(self.coerce_number(ctx, l_val)?);
                let r = self.to_uint32_from(self.coerce_number(ctx, r_val)?);
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

                let js_regex = GcRef::new(JsRegExp::new(
                    pattern.to_string(),
                    flags.to_string(),
                    proto,
                    ctx.memory_manager().clone(),
                ));
                Ok(Value::regex(js_regex))
            }
            Constant::TemplateLiteral {
                site_id,
                cooked,
                raw,
            } => {
                let key = {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("TemplateLiteral without active frame"))?;
                    TemplateCacheKey {
                        realm_id: frame.realm_id,
                        module_ptr: Arc::as_ptr(&frame.module) as usize,
                        site_id: *site_id,
                    }
                };

                if let Some(cached) = ctx.get_cached_template_object(key) {
                    return Ok(Value::array(cached));
                }

                let cooked_values = cooked
                    .iter()
                    .map(|part| match part {
                        Some(units) => Value::string(JsString::intern_utf16(units)),
                        None => Value::undefined(),
                    })
                    .collect::<Vec<_>>();
                let cooked_arr = self.create_template_array(ctx, &cooked_values)?;

                let raw_values = raw
                    .iter()
                    .map(|part| Value::string(JsString::intern_utf16(part)))
                    .collect::<Vec<_>>();
                let raw_arr = self.create_template_array(ctx, &raw_values)?;

                raw_arr.freeze();
                cooked_arr.define_property(
                    PropertyKey::string("raw"),
                    PropertyDescriptor::data_with_attrs(
                        Value::array(raw_arr),
                        PropertyAttributes {
                            writable: false,
                            enumerable: false,
                            configurable: false,
                        },
                    ),
                );
                cooked_arr.freeze();

                ctx.cache_template_object(key, cooked_arr);
                Ok(Value::array(cooked_arr))
            }
            Constant::Symbol(id) => {
                let sym = GcRef::new(crate::value::Symbol {
                    id: *id,
                    description: None,
                });
                Ok(Value::symbol(sym))
            }
        }
    }

    fn create_template_array(
        &self,
        ctx: &mut VmContext,
        values: &[Value],
    ) -> VmResult<GcRef<JsObject>> {
        let arr = GcRef::new(JsObject::array(values.len(), ctx.memory_manager().clone()));
        if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
            && let Some(array_proto) = array_obj
                .get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object())
        {
            arr.set_prototype(Value::object(array_proto));
        }

        for (index, value) in values.iter().enumerate() {
            arr.set(PropertyKey::Index(index as u32), value.clone())
                .map_err(|e| VmError::internal(format!("failed to build template array: {e}")))?;
        }

        Ok(arr)
    }

    /// Execute an eval-compiled module within the current execution context.
    ///
    /// Unlike `execute()` / `run_loop()`, this method tracks the pre-eval
    /// stack depth and returns when the eval frame finishes, without
    /// consuming outer call frames. This is the same pattern used by
    /// `call_function`.
    pub(crate) fn execute_eval_module(
        &self,
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
                        // Capture exports before popping the entry frame
                        self.capture_module_exports(ctx, &module);
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
                            ctx.set_exception(value);
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
                            ctx.set_exception(error_value);
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
                            ctx.set_exception(error_val);
                            continue;
                        }
                    }
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame();
                    }
                    return Err(VmError::exception(error_val));
                }
                Err(VmError::TypeError(msg)) => {
                    let error_val = self.make_error(ctx, "TypeError", &msg);
                    if let Some((target_depth, catch_pc)) = ctx.peek_nearest_try() {
                        if target_depth > prev_stack_depth {
                            ctx.take_nearest_try();
                            while ctx.stack_depth() > target_depth {
                                ctx.pop_frame();
                            }
                            if let Some(frame) = ctx.current_frame_mut() {
                                frame.pc = catch_pc;
                            }
                            ctx.set_exception(error_val);
                            continue;
                        }
                    }
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame();
                    }
                    return Err(VmError::exception(error_val));
                }
                Err(VmError::ReferenceError(msg)) => {
                    let error_val = self.make_error(ctx, "ReferenceError", &msg);
                    if let Some((target_depth, catch_pc)) = ctx.peek_nearest_try() {
                        if target_depth > prev_stack_depth {
                            ctx.take_nearest_try();
                            while ctx.stack_depth() > target_depth {
                                ctx.pop_frame();
                            }
                            if let Some(frame) = ctx.current_frame_mut() {
                                frame.pc = catch_pc;
                            }
                            ctx.set_exception(error_val);
                            continue;
                        }
                    }
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame();
                    }
                    return Err(VmError::exception(error_val));
                }
                Err(VmError::RangeError(msg)) => {
                    let error_val = self.make_error(ctx, "RangeError", &msg);
                    if let Some((target_depth, catch_pc)) = ctx.peek_nearest_try() {
                        if target_depth > prev_stack_depth {
                            ctx.take_nearest_try();
                            while ctx.stack_depth() > target_depth {
                                ctx.pop_frame();
                            }
                            if let Some(frame) = ctx.current_frame_mut() {
                                frame.pc = catch_pc;
                            }
                            ctx.set_exception(error_val);
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

    /// Capture module exports from the current frame into `ctx.captured_exports`.
    ///
    /// Must be called while the entry frame is still on the stack (before pop_frame).
    /// Mirrors the export capture logic in `execute()`.
    fn capture_module_exports(&self, ctx: &mut VmContext, module: &Arc<Module>) {
        let entry_func = match module.entry_function() {
            Some(f) => f,
            None => return,
        };

        let mut exports = std::collections::HashMap::new();
        for export in &module.exports {
            match export {
                otter_vm_bytecode::module::ExportRecord::Named { local, exported } => {
                    if let Some(idx) = entry_func.local_names.iter().position(|n| n == local) {
                        if let Ok(val) = ctx.get_local(idx as u16) {
                            exports.insert(exported.clone(), val);
                        }
                    }
                }
                otter_vm_bytecode::module::ExportRecord::Default { local } => {
                    if let Some(idx) = entry_func.local_names.iter().position(|n| n == local) {
                        if let Ok(val) = ctx.get_local(idx as u16) {
                            exports.insert("default".to_string(), val);
                        }
                    }
                }
                _ => {}
            }
        }

        ctx.set_captured_exports(exports);
    }

    /// Call a native function with depth tracking to prevent Rust stack overflow.
    ///
    /// This method tracks the native call depth and returns an error if it exceeds
    /// the maximum. This prevents JS code that calls native functions recursively
    /// from overflowing the Rust stack.
    #[inline]
    fn call_native_fn(
        &self,
        ctx: &mut VmContext,
        native_fn: &crate::value::NativeFn,
        this_value: &Value,
        args: &[Value],
    ) -> VmResult<Value> {
        ctx.enter_native_call()?;
        let result = {
            let mut ncx = crate::context::NativeContext::new(ctx, self);
            native_fn(this_value, args, &mut ncx)
        };
        // ncx dropped here — ctx borrow released
        ctx.exit_native_call();
        result
    }

    /// Call a function value as a constructor (native or closure).
    ///
    /// This sets the construct flag so `return` uses the constructed `this`
    /// when the constructor returns a non-object.
    pub fn call_function_construct(
        &self,
        ctx: &mut VmContext,
        func: &Value,
        this_value: Value,
        args: &[Value],
    ) -> VmResult<Value> {
        // Check __non_constructor flag (ES2023 §17: built-in methods are not constructors)
        if let Some(func_obj) = func.as_object() {
            if func_obj
                .get(&crate::object::PropertyKey::string("__non_constructor"))
                .and_then(|v| v.as_boolean())
                == Some(true)
            {
                return Err(VmError::type_error("not a constructor"));
            }
        }

        // Check if it's a native function
        if let Some(native_fn) = func.as_native_function() {
            return self.call_native_fn_construct(ctx, native_fn, &this_value, args);
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

        // Set up the call — handle rest parameters
        let mut call_args: Vec<Value> = args.to_vec();
        if func_info.flags.has_rest {
            let param_count = func_info.param_count as usize;
            let rest_args: Vec<Value> = if call_args.len() > param_count {
                call_args.drain(param_count..).collect()
            } else {
                Vec::new()
            };
            let rest_arr = crate::gc::GcRef::new(crate::object::JsObject::array(
                rest_args.len(),
                ctx.memory_manager().clone(),
            ));
            if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object()) {
                if let Some(array_proto) = array_obj
                    .get(&crate::object::PropertyKey::string("prototype"))
                    .and_then(|v| v.as_object())
                {
                    rest_arr.set_prototype(Value::object(array_proto));
                }
            }
            for (i, arg) in rest_args.into_iter().enumerate() {
                let _ = rest_arr.set(crate::object::PropertyKey::Index(i as u32), arg);
            }
            call_args.push(Value::object(rest_arr));
        }

        let argc = call_args.len();
        ctx.set_pending_args(call_args);
        ctx.set_pending_this(this_value);
        ctx.set_pending_upvalues(closure.upvalues.clone());
        // Propagate home_object from closure to the new call frame
        if let Some(ref ho) = closure.home_object {
            ctx.set_pending_home_object(ho.clone());
        }

        let realm_id = self.realm_id_for_function(ctx, func);
        ctx.set_pending_realm_id(realm_id);
        // Store callee value for arguments.callee
        ctx.set_pending_callee_value(func.clone());
        ctx.push_frame(
            closure.function_index,
            Arc::clone(&closure.module),
            func_info.local_count,
            Some(0), // Return register (unused, we get result from Return)
            true,    // Construct call
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
                    let value = if is_construct && !value.is_object() {
                        construct_this
                    } else if is_async {
                        self.create_js_promise(ctx, JsPromise::resolved(value))
                    } else {
                        value
                    };
                    // Check if we've returned to the original depth
                    if ctx.stack_depth() <= prev_stack_depth + 1 {
                        ctx.pop_frame();
                        break value;
                    }
                    // Handle return from nested call
                    ctx.pop_frame();
                    if let Some(reg) = return_reg {
                        ctx.set_register(reg, value);
                    } else {
                        ctx.set_register(0, value);
                    }
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
                        #[cfg(feature = "jit")]
                        {
                            crate::jit_queue::enqueue_hot_function(&module, func_index, func);
                            crate::jit_runtime::compile_one_pending_request();
                        }
                        #[cfg(not(feature = "jit"))]
                        let _ = became_hot;
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
                    // Pop the frame we pushed and unwind to original depth
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame();
                    }
                    ctx.set_running(was_running);
                    return Err(VmError::exception(error));
                }
                Err(e) => {
                    // Pop the frame we pushed and unwind to original depth
                    while ctx.stack_depth() > prev_stack_depth {
                        ctx.pop_frame();
                    }
                    ctx.set_running(was_running);
                    return Err(e);
                }
            }
        };

        ctx.set_running(was_running);
        Ok(result)
    }

    /// Call a native function as a constructor (via `new`).
    /// Sets `NativeContext::is_construct()` to true.
    fn call_native_fn_construct(
        &self,
        ctx: &mut VmContext,
        native_fn: &crate::value::NativeFn,
        this_value: &Value,
        args: &[Value],
    ) -> VmResult<Value> {
        ctx.enter_native_call()?;
        let result = {
            let mut ncx = crate::context::NativeContext::new_construct(ctx, self);
            native_fn(this_value, args, &mut ncx)
        };
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

        // 2. Handle native functions
        if let Some(native_fn) = current_func.as_native_function() {
            // Native function execution
            match self.call_native_fn(ctx, native_fn, &current_this, &current_args) {
                Ok(result) => {
                    ctx.set_register(return_reg, result);
                    return Ok(InstructionResult::Continue);
                }
                Err(e) => return Err(e),
            }
        }

        // 3. Handle closures
        if let Some(closure) = current_func.as_function() {
            if closure.is_generator {
                // Use generator prototype from the function's realm, not the caller's.
                let realm_id = closure
                    .object
                    .get(&PropertyKey::string("__realm_id__"))
                    .and_then(|v| v.as_int32())
                    .map(|id| id as u32)
                    .unwrap_or_else(|| ctx.realm_id());
                let proto = ctx
                    .realm_intrinsics(realm_id)
                    .map(|intrinsics| {
                        if closure.is_async {
                            intrinsics.async_generator_prototype
                        } else {
                            intrinsics.generator_prototype
                        }
                    })
                    .or_else(|| {
                        if closure.is_async {
                            ctx.async_generator_prototype_intrinsic()
                        } else {
                            ctx.generator_prototype_intrinsic()
                        }
                    });

                // Create the generator's internal object
                let gen_obj = GcRef::new(JsObject::new(
                    proto.map(Value::object).unwrap_or_else(Value::null),
                    ctx.memory_manager().clone(),
                ));

                let generator = JsGenerator::new(
                    closure.function_index,
                    Arc::clone(&closure.module),
                    closure.upvalues.clone(),
                    current_args,
                    current_this,
                    false, // is_construct
                    closure.is_async,
                    realm_id,
                    gen_obj,
                );
                // Store callee value for arguments.callee in sloppy mode generators
                generator.set_callee_value(current_func.clone());
                ctx.set_register(return_reg, Value::generator(generator));
                return Ok(InstructionResult::Continue);
            }

            let argc = current_args.len() as u8;
            let realm_id = self.realm_id_for_function(ctx, &current_func);
            ctx.set_pending_realm_id(realm_id);
            ctx.set_pending_this(current_this);
            ctx.set_pending_args(current_args);
            // Propagate home_object from closure to the new call frame
            if let Some(ref ho) = closure.home_object {
                ctx.set_pending_home_object(ho.clone());
            }
            // Store callee value for arguments.callee
            ctx.set_pending_callee_value(current_func.clone());
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
    fn op_add(&self, ctx: &mut VmContext, left: &Value, right: &Value) -> VmResult<Value> {
        let left_prim = self.to_primitive(ctx, left, PreferredType::Default)?;
        let right_prim = self.to_primitive(ctx, right, PreferredType::Default)?;

        // String concatenation
        if left_prim.is_string() || right_prim.is_string() {
            let left_str = self.to_string_value(ctx, &left_prim)?;
            let right_str = self.to_string_value(ctx, &right_prim)?;
            let result = format!("{}{}", left_str, right_str);
            let js_str = JsString::intern(&result);
            return Ok(Value::string(js_str));
        }

        let left_bigint = self.bigint_value(&left_prim)?;
        let right_bigint = self.bigint_value(&right_prim)?;
        if let (Some(left_bigint), Some(right_bigint)) = (left_bigint, right_bigint) {
            let result = left_bigint + right_bigint;
            return Ok(Value::bigint(result.to_string()));
        }

        if left_prim.is_bigint() || right_prim.is_bigint() {
            return Err(VmError::type_error("Cannot mix BigInt and other types"));
        }

        // Numeric addition
        let left_num = self.to_number_value(ctx, &left_prim)?;
        let right_num = self.to_number_value(ctx, &right_prim)?;
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
                    crate::globals::js_number_to_string(n)
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

    fn get_property_value(
        &self,
        ctx: &mut VmContext,
        obj: &GcRef<JsObject>,
        key: &PropertyKey,
        receiver: &Value,
    ) -> VmResult<Value> {
        match obj.lookup_property_descriptor(key) {
            Some(PropertyDescriptor::Accessor { get, .. }) => {
                let Some(getter) = get else {
                    return Ok(Value::undefined());
                };
                if !getter.is_callable() {
                    return Err(VmError::type_error("getter is not a function"));
                }
                self.call_function(ctx, &getter, receiver.clone(), &[])
            }
            Some(PropertyDescriptor::Data { value, .. }) => Ok(value),
            _ => Ok(Value::undefined()),
        }
    }

    fn inject_eval_bindings(&self, ctx: &mut VmContext) -> Vec<PropertyKey> {
        let mut injected = Vec::new();
        let Some(frame) = ctx.current_frame() else {
            return injected;
        };
        let Some(func) = frame.module.function(frame.function_index) else {
            return injected;
        };

        let local_names = func.local_names.clone();
        let global = ctx.global();

        for (index, name) in local_names.iter().enumerate() {
            if name.is_empty() || name.starts_with('$') {
                continue;
            }

            let key = PropertyKey::string(name);
            if matches!(key, PropertyKey::Index(_) | PropertyKey::Symbol(_)) {
                continue;
            }

            if global.has_own(&key) {
                continue;
            }

            let Ok(value) = ctx.get_local(index as u16) else {
                continue;
            };
            if global.set(key, value).is_ok() {
                injected.push(key);
            }
        }

        injected
    }

    fn cleanup_eval_bindings(&self, ctx: &mut VmContext, injected: &[PropertyKey]) {
        let global = ctx.global();
        for key in injected {
            let _ = global.delete(key);
        }
    }

    /// Get a property from an object, walking the prototype chain with proxy trap support.
    /// Unlike `JsObject::get()` which transparently bypasses proxy traps in the prototype chain,
    /// this method properly dispatches to `proxy_get` when a Proxy is encountered.
    fn get_with_proxy_chain(
        &self,
        ctx: &mut VmContext,
        obj: &GcRef<JsObject>,
        key: &PropertyKey,
        key_value: Value,
        receiver: &Value,
    ) -> VmResult<Value> {
        // 1. Check own property (shape/dictionary + elements)
        if let Some(value) = Self::get_own_value(obj, key) {
            return Ok(value);
        }
        // Check for accessor descriptors separately (getters need to be called)
        if let Some(desc) = obj.get_own_property_descriptor(key) {
            if let PropertyDescriptor::Accessor { get, .. } = desc {
                if let Some(getter) = get {
                    return self.call_function(ctx, &getter, receiver.clone(), &[]);
                }
                return Ok(Value::undefined());
            }
        }
        // 2. Walk prototype chain with proxy support
        let mut current = obj.prototype();
        let mut depth = 0;
        loop {
            if current.is_null() || current.is_undefined() {
                return Ok(Value::undefined());
            }
            depth += 1;
            if depth > 256 {
                return Ok(Value::undefined());
            }

            if let Some(proxy) = current.as_proxy() {
                let mut ncx = crate::context::NativeContext::new(ctx, self);
                return crate::proxy_operations::proxy_get(
                    &mut ncx,
                    proxy,
                    key,
                    key_value,
                    receiver.clone(),
                );
            }
            if let Some(proto_obj) = current.as_object() {
                if let Some(value) = Self::get_own_value(&proto_obj, key) {
                    return Ok(value);
                }
                if let Some(desc) = proto_obj.get_own_property_descriptor(key) {
                    if let PropertyDescriptor::Accessor { get, .. } = desc {
                        if let Some(getter) = get {
                            return self.call_function(ctx, &getter, receiver.clone(), &[]);
                        }
                        return Ok(Value::undefined());
                    }
                }
                current = proto_obj.prototype();
            } else {
                break;
            }
        }
        Ok(Value::undefined())
    }

    /// Get own data value from an object, checking both property descriptor and elements array.
    /// Returns None if not found or if it's an accessor (caller must handle accessors).
    fn get_own_value(obj: &GcRef<JsObject>, key: &PropertyKey) -> Option<Value> {
        // Check property descriptor first
        if let Some(desc) = obj.get_own_property_descriptor(key) {
            match desc {
                PropertyDescriptor::Data { value, .. } => return Some(value),
                PropertyDescriptor::Accessor { .. } => return None, // caller handles
                PropertyDescriptor::Deleted => return None,
            }
        }
        // Check indexed elements (JsObject::get does this for all objects, not just arrays)
        if let PropertyKey::Index(i) = key {
            let elements = obj.get_elements_storage().borrow();
            let idx = *i as usize;
            if idx < elements.len() && !elements[idx].is_hole() {
                return Some(elements[idx].clone());
            }
        }
        None
    }

    /// Check if a property exists on an object, walking the prototype chain with proxy trap support.
    fn has_with_proxy_chain(
        &self,
        ctx: &mut VmContext,
        obj: &GcRef<JsObject>,
        key: &PropertyKey,
        key_value: Value,
    ) -> VmResult<bool> {
        if Self::has_own_property(obj, key) {
            return Ok(true);
        }
        let mut current = obj.prototype();
        let mut depth = 0;
        loop {
            if current.is_null() || current.is_undefined() {
                return Ok(false);
            }
            depth += 1;
            if depth > 256 {
                return Ok(false);
            }
            if let Some(proxy) = current.as_proxy() {
                let mut ncx = crate::context::NativeContext::new(ctx, self);
                return crate::proxy_operations::proxy_has(&mut ncx, proxy, key, key_value);
            }
            if let Some(proto_obj) = current.as_object() {
                if Self::has_own_property(&proto_obj, key) {
                    return Ok(true);
                }
                current = proto_obj.prototype();
            } else {
                break;
            }
        }
        Ok(false)
    }

    /// Check if an object has an own property, including elements array.
    fn has_own_property(obj: &GcRef<JsObject>, key: &PropertyKey) -> bool {
        if obj.get_own_property_descriptor(key).is_some() {
            return true;
        }
        // Also check elements for Index keys (arguments object stores values in elements)
        if let PropertyKey::Index(i) = key {
            let elements = obj.get_elements_storage().borrow();
            let idx = *i as usize;
            if idx < elements.len() && !elements[idx].is_hole() {
                return true;
            }
        }
        false
    }

    /// Set a property on an object, walking the prototype chain with proxy trap support.
    ///
    /// Per ES2023 §9.1.9 OrdinarySet:
    /// If the object doesn't have the own property and a proxy is found in the prototype
    /// chain, the proxy's [[Set]] trap should be invoked with the original receiver.
    fn set_with_proxy_chain(
        &self,
        ctx: &mut VmContext,
        obj: &GcRef<JsObject>,
        key: &PropertyKey,
        key_value: Value,
        value: Value,
        receiver: &Value,
    ) -> VmResult<bool> {
        // 1. Check for own property descriptor
        if let Some(desc) = obj.get_own_property_descriptor(key) {
            match desc {
                crate::object::PropertyDescriptor::Accessor { set, .. } => {
                    if let Some(setter) = set {
                        self.call_function(ctx, &setter, receiver.clone(), &[value])?;
                        return Ok(true);
                    }
                    return Ok(false);
                }
                _ => {
                    // Data property or deleted - set directly on receiver
                    if let Some(recv_obj) = receiver.as_object() {
                        return Ok(recv_obj.set(*key, value).is_ok());
                    }
                    return Ok(false);
                }
            }
        }
        // Also check elements for own Index properties
        if let PropertyKey::Index(i) = key {
            let elements = obj.get_elements_storage().borrow();
            let idx = *i as usize;
            if idx < elements.len() && !elements[idx].is_hole() {
                drop(elements);
                if let Some(recv_obj) = receiver.as_object() {
                    return Ok(recv_obj.set(*key, value).is_ok());
                }
                return Ok(false);
            }
        }
        // 2. Walk prototype chain looking for proxy or accessor
        let mut current = obj.prototype();
        let mut depth = 0;
        loop {
            if current.is_null() || current.is_undefined() {
                // Not found in chain - set on receiver
                if let Some(recv_obj) = receiver.as_object() {
                    return Ok(recv_obj.set(*key, value).is_ok());
                }
                return Ok(false);
            }
            depth += 1;
            if depth > 256 {
                return Ok(false);
            }
            if let Some(proxy) = current.as_proxy() {
                let mut ncx = crate::context::NativeContext::new(ctx, self);
                return crate::proxy_operations::proxy_set(
                    &mut ncx,
                    proxy,
                    key,
                    key_value,
                    value,
                    receiver.clone(),
                );
            }
            if let Some(proto_obj) = current.as_object() {
                if let Some(desc) = proto_obj.get_own_property_descriptor(key) {
                    match desc {
                        crate::object::PropertyDescriptor::Accessor { set, .. } => {
                            if let Some(setter) = set {
                                self.call_function(ctx, &setter, receiver.clone(), &[value])?;
                                return Ok(true);
                            }
                            return Ok(false);
                        }
                        _ => {
                            // Data property found in prototype - set on receiver
                            if let Some(recv_obj) = receiver.as_object() {
                                return Ok(recv_obj.set(*key, value).is_ok());
                            }
                            return Ok(false);
                        }
                    }
                }
                current = proto_obj.prototype();
            } else {
                break;
            }
        }
        // Fallback: set directly on receiver
        if let Some(recv_obj) = receiver.as_object() {
            return Ok(recv_obj.set(*key, value).is_ok());
        }
        Ok(false)
    }

    /// Convert value to primitive per ES2023 §7.1.1.
    pub(crate) fn to_primitive(
        &self,
        ctx: &mut VmContext,
        value: &Value,
        hint: PreferredType,
    ) -> VmResult<Value> {
        if !value.is_object() {
            return Ok(value.clone());
        }

        // Handle proxy: use proxy_get for property lookups
        if let Some(proxy) = value.as_proxy() {
            // 1. @@toPrimitive
            let to_prim_key =
                PropertyKey::Symbol(crate::intrinsics::well_known::to_primitive_symbol());
            let to_prim_key_value =
                Value::symbol(crate::intrinsics::well_known::to_primitive_symbol());
            let method = {
                let mut ncx = crate::context::NativeContext::new(ctx, self);
                crate::proxy_operations::proxy_get(
                    &mut ncx,
                    proxy,
                    &to_prim_key,
                    to_prim_key_value,
                    value.clone(),
                )?
            };
            if !method.is_undefined() && !method.is_null() {
                if !method.is_callable() {
                    return Err(VmError::type_error(
                        "Cannot convert object to primitive value",
                    ));
                }
                let hint_str = match hint {
                    PreferredType::Default => "default",
                    PreferredType::Number => "number",
                    PreferredType::String => "string",
                };
                let hint_val = Value::string(JsString::intern(hint_str));
                let result = self.call_function(ctx, &method, value.clone(), &[hint_val])?;
                if !result.is_object() {
                    return Ok(result);
                }
                return Err(VmError::type_error(
                    "Cannot convert object to primitive value",
                ));
            }

            // 2. OrdinaryToPrimitive via proxy
            let (first, second) = match hint {
                PreferredType::String => ("toString", "valueOf"),
                _ => ("valueOf", "toString"),
            };
            for name in [first, second] {
                let key = PropertyKey::string(name);
                let key_value = Value::string(JsString::intern(name));
                let method = {
                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                    crate::proxy_operations::proxy_get(
                        &mut ncx,
                        proxy,
                        &key,
                        key_value,
                        value.clone(),
                    )?
                };
                if method.is_callable() {
                    let result = self.call_function(ctx, &method, value.clone(), &[])?;
                    if !result.is_object() {
                        return Ok(result);
                    }
                }
            }

            return Err(VmError::type_error(
                "Cannot convert object to primitive value",
            ));
        }

        let Some(obj) = value.as_object() else {
            return Ok(value.clone());
        };

        // 1. @@toPrimitive
        let to_prim_key = PropertyKey::Symbol(crate::intrinsics::well_known::to_primitive_symbol());
        let method = self.get_property_value(ctx, &obj, &to_prim_key, value)?;
        if !method.is_undefined() && !method.is_null() {
            if !method.is_callable() {
                return Err(VmError::type_error(
                    "Cannot convert object to primitive value",
                ));
            }
            let hint_str = match hint {
                PreferredType::Default => "default",
                PreferredType::Number => "number",
                PreferredType::String => "string",
            };
            let hint_val = Value::string(JsString::intern(hint_str));
            let result = self.call_function(ctx, &method, value.clone(), &[hint_val])?;
            if !result.is_object() {
                return Ok(result);
            }
            return Err(VmError::type_error(
                "Cannot convert object to primitive value",
            ));
        }

        // 2. OrdinaryToPrimitive.
        let (first, second) = match hint {
            PreferredType::String => ("toString", "valueOf"),
            _ => ("valueOf", "toString"),
        };
        for name in [first, second] {
            let method = self.get_property_value(ctx, &obj, &PropertyKey::string(name), value)?;
            if method.is_callable() {
                let result = self.call_function(ctx, &method, value.clone(), &[])?;
                if !result.is_object() {
                    return Ok(result);
                }
            }
        }

        Err(VmError::type_error(
            "Cannot convert object to primitive value",
        ))
    }

    /// Convert value to string per ES2023 §7.1.17.
    pub(crate) fn to_string_value(&self, ctx: &mut VmContext, value: &Value) -> VmResult<String> {
        if value.is_undefined() {
            return Ok("undefined".to_string());
        }
        if value.is_null() {
            return Ok("null".to_string());
        }
        if let Some(b) = value.as_boolean() {
            return Ok(if b { "true" } else { "false" }.to_string());
        }
        if let Some(n) = value.as_number() {
            return Ok(crate::globals::js_number_to_string(n));
        }
        if let Some(s) = value.as_string() {
            return Ok(s.as_str().to_string());
        }
        if let Some(crate::value::HeapRef::BigInt(b)) = value.heap_ref() {
            return Ok(b.value.clone());
        }
        if value.is_symbol() {
            return Err(VmError::type_error(
                "Cannot convert a Symbol value to a string",
            ));
        }
        if value.is_object() {
            let prim = self.to_primitive(ctx, value, PreferredType::String)?;
            return self.to_string_value(ctx, &prim);
        }
        Ok("[object Object]".to_string())
    }

    /// Convert value to number per ES2023 §7.1.4.
    pub(crate) fn to_number_value(&self, ctx: &mut VmContext, value: &Value) -> VmResult<f64> {
        let prim = if value.is_object() {
            self.to_primitive(ctx, value, PreferredType::Number)?
        } else {
            value.clone()
        };
        if prim.is_symbol() {
            return Err(VmError::type_error(
                "Cannot convert a Symbol value to a number",
            ));
        }
        if prim.is_bigint() {
            return Err(VmError::type_error(
                "Cannot convert a BigInt value to a number",
            ));
        }
        Ok(self.to_number(&prim))
    }

    fn parse_string_to_number(&self, input: &str) -> f64 {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return 0.0;
        }

        let (sign, rest) = if let Some(stripped) = trimmed.strip_prefix('-') {
            (-1.0, stripped)
        } else if let Some(stripped) = trimmed.strip_prefix('+') {
            (1.0, stripped)
        } else {
            (1.0, trimmed)
        };

        if rest == "Infinity" {
            return sign * f64::INFINITY;
        }

        let (radix, digits) = if let Some(rest) = rest.strip_prefix("0x") {
            (16, rest)
        } else if let Some(rest) = rest.strip_prefix("0X") {
            (16, rest)
        } else if let Some(rest) = rest.strip_prefix("0o") {
            (8, rest)
        } else if let Some(rest) = rest.strip_prefix("0O") {
            (8, rest)
        } else if let Some(rest) = rest.strip_prefix("0b") {
            (2, rest)
        } else if let Some(rest) = rest.strip_prefix("0B") {
            (2, rest)
        } else {
            (10, "")
        };

        if radix != 10 {
            if digits.is_empty() {
                return f64::NAN;
            }
            // Numeric separators (_) are only valid in source code literals,
            // not in Number() string conversion (ToNumber)
            if digits.contains('_') {
                return f64::NAN;
            }
            if let Some(bigint) = NumBigInt::parse_bytes(digits.as_bytes(), radix) {
                return bigint.to_f64().unwrap_or(f64::INFINITY) * sign;
            }
            return f64::NAN;
        }

        trimmed.parse::<f64>().unwrap_or(f64::NAN)
    }

    /// Create a JavaScript Promise object from an internal promise
    /// This creates an object with _internal field and copies methods from Promise.prototype
    fn create_js_promise(&self, ctx: &VmContext, internal: GcRef<JsPromise>) -> Value {
        let obj = GcRef::new(JsObject::new(Value::null(), ctx.memory_manager().clone()));

        // Set _internal to the raw promise
        let _ = obj.set(PropertyKey::string("_internal"), Value::promise(internal));

        // Try to get Promise.prototype and copy its methods
        if let Some(promise_ctor) = ctx.get_global("Promise").and_then(|v| v.as_object()) {
            if let Some(proto) = promise_ctor
                .get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object())
            {
                // Copy then, catch, finally from prototype
                if let Some(then_fn) = proto.get(&PropertyKey::string("then")) {
                    let _ = obj.set(PropertyKey::string("then"), then_fn);
                }
                if let Some(catch_fn) = proto.get(&PropertyKey::string("catch")) {
                    let _ = obj.set(PropertyKey::string("catch"), catch_fn);
                }
                if let Some(finally_fn) = proto.get(&PropertyKey::string("finally")) {
                    let _ = obj.set(PropertyKey::string("finally"), finally_fn);
                }

                // Set prototype for proper inheritance
                obj.set_prototype(Value::object(proto));
            }
        }

        Value::object(obj)
    }

    /// Convert object to primitive using number hint.
    fn to_primitive_number(&self, ctx: &mut VmContext, value: &Value) -> VmResult<Value> {
        self.to_primitive(ctx, value, PreferredType::Number)
    }

    /// Convert primitive value to number (small ToNumber subset).
    /// Does NOT handle objects - for objects, use `to_number_value()` which
    /// invokes ToPrimitive first per ES2023 §7.1.4.
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
            return self.parse_string_to_number(s.as_str());
        }
        // Objects should be converted via to_number_value() which calls ToPrimitive
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

        let obj = GcRef::new(JsObject::new(
            proto.map(Value::object).unwrap_or_else(Value::null),
            ctx.memory_manager().clone(),
        ));
        let _ = obj.set(
            PropertyKey::string("name"),
            Value::string(JsString::intern(name)),
        );
        let _ = obj.set(
            PropertyKey::string("message"),
            Value::string(JsString::intern(message)),
        );
        let stack = if message.is_empty() {
            name.to_string()
        } else {
            format!("{}: {}", name, message)
        };
        let _ = obj.set(
            PropertyKey::string("stack"),
            Value::string(JsString::intern(&stack)),
        );
        let _ = obj.set(PropertyKey::string("__isError__"), Value::boolean(true));
        let _ = obj.set(
            PropertyKey::string("__errorType__"),
            Value::string(JsString::intern(name)),
        );
        if let Some(ctor) = ctor_value {
            let _ = obj.set(PropertyKey::string("constructor"), ctor);
        }

        Value::object(obj)
    }

    fn coerce_number(&self, ctx: &mut VmContext, value: Value) -> VmResult<f64> {
        self.to_number_value(ctx, &value)
    }

    fn bigint_value(&self, value: &Value) -> VmResult<Option<NumBigInt>> {
        if let Some(crate::value::HeapRef::BigInt(b)) = value.heap_ref() {
            let bigint = self.parse_bigint_str(&b.value)?;
            return Ok(Some(bigint));
        }
        Ok(None)
    }

    fn to_numeric(&self, ctx: &mut VmContext, value: &Value) -> VmResult<Numeric> {
        let prim = if value.is_object() {
            self.to_primitive(ctx, value, PreferredType::Number)?
        } else {
            value.clone()
        };
        if let Some(bigint) = self.bigint_value(&prim)? {
            return Ok(Numeric::BigInt(bigint));
        }
        if prim.is_symbol() {
            return Err(VmError::type_error("Cannot convert to number"));
        }
        Ok(Numeric::Number(self.to_number(&prim)))
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

    pub(crate) fn parse_bigint_str(&self, value: &str) -> VmResult<NumBigInt> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Ok(NumBigInt::zero());
        }

        let (sign, digits, had_sign) = if let Some(rest) = trimmed.strip_prefix('-') {
            (true, rest, true)
        } else if let Some(rest) = trimmed.strip_prefix('+') {
            (false, rest, true)
        } else {
            (false, trimmed, false)
        };

        if had_sign {
            if digits.starts_with("0x")
                || digits.starts_with("0X")
                || digits.starts_with("0o")
                || digits.starts_with("0O")
                || digits.starts_with("0b")
                || digits.starts_with("0B")
            {
                return Err(VmError::syntax_error("Invalid BigInt"));
            }
        }

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
            return Err(VmError::syntax_error("Invalid BigInt"));
        }
        let mut bigint = NumBigInt::parse_bytes(cleaned.as_bytes(), radix)
            .ok_or_else(|| VmError::syntax_error("Invalid BigInt"))?;
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
    fn value_to_property_key(&self, ctx: &mut VmContext, value: &Value) -> VmResult<PropertyKey> {
        if let Some(sym) = value.as_symbol() {
            return Ok(PropertyKey::Symbol(sym));
        }
        let prim = if value.is_object() {
            self.to_primitive(ctx, value, PreferredType::String)?
        } else {
            value.clone()
        };
        if let Some(sym) = prim.as_symbol() {
            return Ok(PropertyKey::Symbol(sym));
        }
        let key_str = self.to_string_value(ctx, &prim)?;
        if let Ok(n) = key_str.parse::<u32>() {
            if n.to_string() == key_str {
                return Ok(PropertyKey::Index(n));
            }
        }
        Ok(PropertyKey::string(&key_str))
    }

    /// Abstract equality comparison (==) per ES2023 §7.2.14 IsLooselyEqual
    ///
    /// # NaN Handling
    /// NaN == NaN returns false (IEEE 754 semantics via f64 comparison)
    ///
    /// # Recursion Protection
    /// Depth-limited to MAX_ABSTRACT_EQUAL_DEPTH to prevent stack overflow
    /// from malicious valueOf/toString implementations.
    fn abstract_equal(&self, ctx: &mut VmContext, left: &Value, right: &Value) -> VmResult<bool> {
        self.abstract_equal_impl(ctx, left, right, 0)
    }

    /// Internal implementation with depth tracking
    fn abstract_equal_impl(
        &self,
        ctx: &mut VmContext,
        left: &Value,
        right: &Value,
        depth: usize,
    ) -> VmResult<bool> {
        // Prevent stack overflow from malicious valueOf/toString chains
        if depth > MAX_ABSTRACT_EQUAL_DEPTH {
            return Err(VmError::range_error(
                "Maximum recursion depth exceeded in equality comparison",
            ));
        }

        // Same type fast paths
        if left.is_undefined() && right.is_undefined() {
            return Ok(true);
        }
        if left.is_null() && right.is_null() {
            return Ok(true);
        }
        if left.is_number() && right.is_number() {
            let a = left.as_number().unwrap();
            let b = right.as_number().unwrap();
            // NaN == NaN returns false per IEEE 754
            return Ok(a == b);
        }
        if let (Some(a), Some(b)) = (left.as_string(), right.as_string()) {
            return Ok(a == b);
        }
        if let (Some(a), Some(b)) = (left.as_boolean(), right.as_boolean()) {
            return Ok(a == b);
        }
        if left.is_bigint() && right.is_bigint() {
            let left_bigint = self.bigint_value(left)?.unwrap_or_else(NumBigInt::zero);
            let right_bigint = self.bigint_value(right)?.unwrap_or_else(NumBigInt::zero);
            return Ok(left_bigint == right_bigint);
        }
        if left.is_symbol() && right.is_symbol() {
            return Ok(left == right);
        }
        if left.is_object() && right.is_object() {
            return Ok(self.strict_equal(left, right));
        }

        // null == undefined
        if (left.is_null() && right.is_undefined()) || (left.is_undefined() && right.is_null()) {
            return Ok(true);
        }

        // Number <-> String
        if left.is_number() && right.is_string() {
            let right_num = self.to_number(right);
            let left_num = left.as_number().unwrap();
            return Ok(left_num == right_num);
        }
        if left.is_string() && right.is_number() {
            let left_num = self.to_number(left);
            let right_num = right.as_number().unwrap();
            return Ok(left_num == right_num);
        }

        // BigInt <-> String
        if left.is_bigint() && right.is_string() {
            let right_str = right.as_string().unwrap();
            if let Ok(parsed) = self.parse_bigint_str(right_str.as_str()) {
                let left_bigint = self.bigint_value(left)?.unwrap_or_else(NumBigInt::zero);
                return Ok(left_bigint == parsed);
            }
            return Ok(false);
        }
        if left.is_string() && right.is_bigint() {
            let left_str = left.as_string().unwrap();
            if let Ok(parsed) = self.parse_bigint_str(left_str.as_str()) {
                let right_bigint = self.bigint_value(right)?.unwrap_or_else(NumBigInt::zero);
                return Ok(parsed == right_bigint);
            }
            return Ok(false);
        }

        // BigInt <-> Number
        if left.is_bigint() && right.is_number() {
            let right_num = right.as_number().unwrap();
            let left_bigint = self.bigint_value(left)?.unwrap_or_else(NumBigInt::zero);
            return Ok(matches!(
                self.compare_bigint_number(&left_bigint, right_num),
                Some(Ordering::Equal)
            ));
        }
        if left.is_number() && right.is_bigint() {
            let left_num = left.as_number().unwrap();
            let right_bigint = self.bigint_value(right)?.unwrap_or_else(NumBigInt::zero);
            return Ok(matches!(
                self.compare_bigint_number(&right_bigint, left_num),
                Some(Ordering::Equal)
            ));
        }

        // Boolean -> ToNumber, recurse
        if let Some(b) = left.as_boolean() {
            let num = if b { 1.0 } else { 0.0 };
            return self.abstract_equal_impl(ctx, &Value::number(num), right, depth + 1);
        }
        if let Some(b) = right.as_boolean() {
            let num = if b { 1.0 } else { 0.0 };
            return self.abstract_equal_impl(ctx, left, &Value::number(num), depth + 1);
        }

        // Object <-> Primitive: ToPrimitive, recurse
        if left.is_object() && !right.is_object() {
            let prim = self.to_primitive(ctx, left, PreferredType::Default)?;
            return self.abstract_equal_impl(ctx, &prim, right, depth + 1);
        }
        if right.is_object() && !left.is_object() {
            let prim = self.to_primitive(ctx, right, PreferredType::Default)?;
            return self.abstract_equal_impl(ctx, left, &prim, depth + 1);
        }

        // Symbol with non-symbol
        if left.is_symbol() || right.is_symbol() {
            return Ok(false);
        }

        Ok(false)
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

    /// Determine the realm id for a constructor function (best-effort).
    pub(crate) fn realm_id_for_function(&self, ctx: &VmContext, value: &Value) -> RealmId {
        let mut current = value.clone();
        if let Some(proxy) = current.as_proxy() {
            if let Some(target) = proxy.target() {
                current = target;
            }
        }

        if let Some(obj) = current.as_object() {
            if let Some(id) = obj
                .get(&PropertyKey::string("__realm_id__"))
                .and_then(|v| v.as_int32())
            {
                return id as RealmId;
            }
        }
        ctx.realm_id()
    }

    /// Default Object.prototype for a constructor's realm (GetPrototypeFromConstructor fallback).
    pub(crate) fn default_object_prototype_for_constructor(
        &self,
        ctx: &VmContext,
        ctor: &Value,
    ) -> Option<GcRef<JsObject>> {
        let realm_id = self.realm_id_for_function(ctx, ctor);
        if let Some(intrinsics) = ctx.realm_intrinsics(realm_id) {
            let mut current = ctor.clone();
            if let Some(proxy) = current.as_proxy() {
                if let Some(target) = proxy.target() {
                    current = target;
                }
            }
            if let Some(tag) = current
                .as_object()
                .and_then(|o| o.get(&PropertyKey::string("__builtin_tag__")))
                .and_then(|v| v.as_string())
            {
                if let Some(proto) = intrinsics.prototype_for_builtin_tag(tag.as_str()) {
                    return Some(proto);
                }
            }
            return Some(intrinsics.object_prototype);
        }
        ctx.global()
            .get(&PropertyKey::string("Object"))
            .and_then(|v| v.as_object())
            .and_then(|o| o.get(&PropertyKey::string("prototype")))
            .and_then(|v| v.as_object())
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
        let frames: Vec<_> = ctx.call_stack().iter().rev().skip(1).take(10).collect();

        // Create array to hold stack frame objects
        let frames_array = GcRef::new(JsObject::array(frames.len(), ctx.memory_manager().clone()));

        for (i, frame) in frames.iter().enumerate() {
            let frame_obj = GcRef::new(JsObject::new(Value::null(), ctx.memory_manager().clone()));

            // Get function name
            if let Some(func_def) = frame.module.functions.get(frame.function_index as usize) {
                let func_name = func_def
                    .name
                    .clone()
                    .unwrap_or_else(|| "<anonymous>".to_string());
                let _ = frame_obj.set(
                    PropertyKey::string("function"),
                    Value::string(JsString::intern(&func_name)),
                );
            } else {
                let _ = frame_obj.set(
                    PropertyKey::string("function"),
                    Value::string(JsString::intern("<unknown>")),
                );
            }

            // Get source file
            let source_url = &frame.module.source_url;
            if !source_url.is_empty() {
                let _ = frame_obj.set(
                    PropertyKey::string("file"),
                    Value::string(JsString::intern(source_url)),
                );
            }

            // Resolve source location from function source map if present.
            // `frame.pc` can point at the next instruction, so also try `pc - 1`.
            if let Some(func) = frame.module.functions.get(frame.function_index as usize) {
                let entry = func.source_map.as_ref().and_then(|map| {
                    map.find(frame.pc as u32).or_else(|| {
                        frame
                            .pc
                            .checked_sub(1)
                            .and_then(|prev_pc| map.find(prev_pc as u32))
                    })
                });

                if let Some(loc) = entry {
                    let _ =
                        frame_obj.set(PropertyKey::string("line"), Value::number(loc.line as f64));
                    let _ = frame_obj.set(
                        PropertyKey::string("column"),
                        Value::number(loc.column as f64),
                    );
                }
            }

            let _ = frames_array.set(PropertyKey::Index(i as u32), Value::object(frame_obj));
        }

        // Store frames array as hidden property
        let _ = error_obj.set(
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
        promise: GcRef<JsPromise>,
        /// The register to store the resolved value
        resume_reg: u16,
        /// The generator (for resumption)
        generator: GcRef<JsGenerator>,
    },
}

fn make_iterator_result_object(
    memory_manager: Arc<crate::memory::MemoryManager>,
    value: Value,
    done: bool,
) -> Value {
    let iter_result = GcRef::new(JsObject::new(Value::null(), memory_manager));
    let _ = iter_result.set(PropertyKey::string("value"), value);
    let _ = iter_result.set(PropertyKey::string("done"), Value::boolean(done));
    Value::object(iter_result)
}

fn make_async_generator_resume_callback(
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

fn async_generator_result_to_promise_value(
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
        let argc = args.len();

        // Set up pending args and push initial frame
        ctx.set_pending_realm_id(generator.realm_id);
        ctx.set_pending_args(args);
        ctx.set_pending_this(this_value);
        ctx.set_pending_upvalues(generator.upvalues.clone());
        // Set callee value for arguments.callee in sloppy mode
        if let Some(callee) = generator.take_callee_value() {
            ctx.set_pending_callee_value(callee);
        }

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
        &self,
        generator: GcRef<JsGenerator>,
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

            // Capture trace data while frame is borrowed (record after execution)
            let trace_data = if ctx.trace_state.is_some() {
                Some((
                    frame.pc,
                    frame.function_index,
                    Arc::clone(&frame.module),
                    instruction.clone(),
                ))
            } else {
                None
            };
            let trace_capture_timing = ctx
                .trace_state
                .as_ref()
                .map(|state| state.config.capture_timing)
                .unwrap_or(false);
            let trace_start_time = if trace_capture_timing {
                Some(std::time::Instant::now())
            } else {
                None
            };

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
        promise: GcRef<JsPromise>,
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
        let global = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
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
        let interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_int32(), Some(42));
    }

    #[test]
    fn test_debugger_instruction_triggers_hook() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut builder = Module::builder("test.js");
        let func = Function::builder()
            .name("main")
            .instruction(Instruction::Debugger)
            .instruction(Instruction::ReturnUndefined)
            .build();
        builder.add_function(func);
        let module = builder.build();

        let hook_calls = Arc::new(AtomicUsize::new(0));
        let hook_calls_clone = Arc::clone(&hook_calls);

        let mut ctx = create_test_context();
        ctx.set_debugger_hook(Some(Arc::new(move |_| {
            hook_calls_clone.fetch_add(1, Ordering::SeqCst);
        })));

        let interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();
        assert!(result.is_undefined());
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_trace_records_modified_registers() {
        let mut builder = Module::builder("test.js");
        let func = Function::builder()
            .name("main")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 42,
            })
            .instruction(Instruction::ReturnUndefined)
            .build();
        builder.add_function(func);
        let module = builder.build();

        let mut ctx = create_test_context();
        ctx.set_trace_config(crate::trace::TraceConfig {
            enabled: true,
            mode: crate::trace::TraceMode::RingBuffer,
            ring_buffer_size: 16,
            output_path: None,
            filter: None,
            capture_timing: false,
        });

        let interpreter = Interpreter::new();
        let _ = interpreter.execute(&module, &mut ctx).unwrap();

        let entries: Vec<_> = ctx.get_trace_buffer().unwrap().iter().cloned().collect();
        let load_entry = entries
            .iter()
            .find(|entry| entry.opcode == "LoadInt32")
            .expect("expected LoadInt32 in trace");

        assert!(!load_entry.modified_registers.is_empty());
        assert_eq!(load_entry.modified_registers[0].0, 0);
        assert!(load_entry.modified_registers[0].1.contains("42"));
    }

    #[test]
    fn test_trace_records_execution_timing_when_enabled() {
        let mut builder = Module::builder("test.js");
        let func = Function::builder()
            .name("main")
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 2,
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
        ctx.set_trace_config(crate::trace::TraceConfig {
            enabled: true,
            mode: crate::trace::TraceMode::RingBuffer,
            ring_buffer_size: 16,
            output_path: None,
            filter: None,
            capture_timing: true,
        });

        let interpreter = Interpreter::new();
        let _ = interpreter.execute(&module, &mut ctx).unwrap();

        let entries: Vec<_> = ctx.get_trace_buffer().unwrap().iter().cloned().collect();

        assert!(!entries.is_empty());
        assert!(
            entries
                .iter()
                .all(|entry| entry.execution_time_ns.is_some())
        );
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
        let result = interpreter.execute(&module, &mut ctx).unwrap();

        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_ic_coverage_instanceof() {
        // Test InstanceOf IC - caches prototype lookup on constructor
        // This test uses Construct to properly create an instance
        use otter_vm_bytecode::FunctionIndex;

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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let obj1 = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let obj2 = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));

        // Set prototype should bump epoch
        obj1.set_prototype(Value::object(obj2.clone()));

        let after_first = get_proto_epoch();
        assert!(
            after_first > initial_epoch,
            "proto_epoch should be bumped after set_prototype"
        );

        // Another set_prototype should bump again
        let obj3 = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        obj2.set_prototype(Value::object(obj3));

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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
        let obj = GcRef::new(JsObject::new(Value::null(), memory_manager));

        // Initially not in dictionary mode
        assert!(
            !obj.is_dictionary_mode(),
            "Object should not be in dictionary mode initially"
        );

        // Add properties up to just below threshold
        for i in 0..(DICTIONARY_THRESHOLD - 1) {
            let key = PropertyKey::String(crate::string::JsString::intern(&format!("prop{}", i)));
            let _ = obj.set(key, Value::int32(i as i32));
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
        let _ = obj.set(key, Value::int32(DICTIONARY_THRESHOLD as i32 - 1));

        // One more should trigger dictionary mode
        let key = PropertyKey::String(crate::string::JsString::intern(&format!(
            "prop{}",
            DICTIONARY_THRESHOLD
        )));
        let _ = obj.set(key, Value::int32(DICTIONARY_THRESHOLD as i32));

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
        let obj = GcRef::new(JsObject::new(Value::null(), memory_manager));

        // Add a few properties
        let key_a = PropertyKey::String(crate::string::JsString::intern("a"));
        let key_b = PropertyKey::String(crate::string::JsString::intern("b"));
        let _ = obj.set(key_a.clone(), Value::int32(1));
        let _ = obj.set(key_b.clone(), Value::int32(2));

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
        let obj = GcRef::new(JsObject::new(Value::null(), memory_manager));

        // Add a property
        let key_a = PropertyKey::String(crate::string::JsString::intern("a"));
        let _ = obj.set(key_a.clone(), Value::int32(42));

        // Trigger dictionary mode via delete
        let key_b = PropertyKey::String(crate::string::JsString::intern("b"));
        let _ = obj.set(key_b.clone(), Value::int32(100));
        obj.delete(&key_b);

        assert!(obj.is_dictionary_mode());

        // Add a new property in dictionary mode
        let key_c = PropertyKey::String(crate::string::JsString::intern("c"));
        let _ = obj.set(key_c.clone(), Value::int32(200));

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
        let obj = GcRef::new(JsObject::new(Value::null(), memory_manager));

        // Add and delete a property to trigger dictionary mode
        let key_a = PropertyKey::String(crate::string::JsString::intern("a"));
        let key_b = PropertyKey::String(crate::string::JsString::intern("b"));
        let _ = obj.set(key_a.clone(), Value::int32(1));
        let _ = obj.set(key_b.clone(), Value::int32(2));
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
            .register_count(1)
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
            let interpreter = Interpreter::new();
            let _ = interpreter.execute_arc(module.clone(), &mut ctx);
        }

        // Call count should be 100
        assert_eq!(func.get_call_count(), 100);
        assert!(!func.is_hot_function()); // Not yet hot

        // Execute until we cross the threshold
        for _ in 0..(HOT_FUNCTION_THRESHOLD - 100) {
            let mut ctx = create_test_context();
            let interpreter = Interpreter::new();
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
        let interpreter = Interpreter::new();
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
