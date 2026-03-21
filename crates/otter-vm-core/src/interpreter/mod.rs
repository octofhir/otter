//! Bytecode interpreter
//!
//! Executes bytecode instructions.

use otter_vm_bytecode::operand::{ConstantIndex, Register};
use otter_vm_bytecode::{Instruction, Module, TypeFlags, UpvalueCapture};

use crate::async_context::{AsyncContext, VmExecutionResult};
use crate::context::{DispatchAction, TemplateCacheKey, VmContext};
use crate::error::{VmError, VmResult};
use crate::gc::GcRef;
use crate::generator::{GeneratorFrame, GeneratorState, JsGenerator};
use crate::memory::MemoryManager;
use crate::object::{
    JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey, SetPropertyError,
    get_proto_epoch,
};
use crate::promise::{JsPromise, JsPromiseJob, JsPromiseJobKind, PromiseState};
use crate::realm::RealmId;
use crate::regexp::JsRegExp;
use crate::string::JsString;
use crate::typed_array_ops::{self, TaHasResult, TaSetResult};
use crate::value::{Closure, NativeFn, UpvalueCell, Value};

use num_bigint::BigInt as NumBigInt;
use num_traits::{One, ToPrimitive, Zero};
use smallvec::SmallVec;
use std::cmp::Ordering;
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

mod call_dispatch;
mod coercion;
mod eval;
mod property;
pub(crate) use coercion::Numeric;

#[derive(Copy, Clone, Debug)]
pub(crate) enum PreferredType {
    Default,
    Number,
    String,
}

mod jit;
use jit::BackEdgeOsrOutcome;
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
        | Instruction::GetElemInt { dst, .. }
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
        | Instruction::Import { dst, .. }
        // Quickened variants
        | Instruction::AddInt32 { dst, .. }
        | Instruction::SubInt32 { dst, .. }
        | Instruction::MulInt32 { dst, .. }
        | Instruction::DivInt32 { dst, .. }
        | Instruction::AddNumber { dst, .. }
        | Instruction::SubNumber { dst, .. }
        | Instruction::GetPropQuickened { dst, .. }
        | Instruction::GetPropString { dst, .. }
        | Instruction::GetArrayLength { dst, .. }
        | Instruction::GetLocalProp { dst, .. } => vec![dst.0],

        Instruction::GetLocal2 { dst1, dst2, .. } => vec![dst1.0, dst2.0],

        _ => vec![],
    }
}

fn trace_modified_registers(instruction: &Instruction, ctx: &VmContext) -> Vec<(u16, String)> {
    const TRACE_VALUE_PREVIEW_LIMIT: usize = 160;

    #[inline]
    fn truncate_debug_value(mut raw: String) -> String {
        if raw.len() <= TRACE_VALUE_PREVIEW_LIMIT {
            return raw;
        }

        let mut end = TRACE_VALUE_PREVIEW_LIMIT;
        while !raw.is_char_boundary(end) {
            end -= 1;
        }
        raw.truncate(end);
        raw.push_str("...");
        raw
    }

    trace_modified_register_indices(instruction)
        .into_iter()
        .map(|reg| {
            let raw = format!("{:?}", ctx.get_register(reg));
            (reg, truncate_debug_value(raw))
        })
        .collect()
}

/// Walk the prototype chain to `depth` and read the property at `offset`.
///
/// Used by the IC hit path when `depth > 0` and the proto_epoch guard
/// has confirmed the cached entry is still valid.
#[inline]
fn get_proto_value_at_depth(obj: &GcRef<JsObject>, depth: u8, offset: u32) -> Option<Value> {
    let mut current = obj.prototype();
    for _ in 1..depth {
        current = current.as_object()?.prototype();
    }
    current.as_object()?.get_by_offset(offset as usize)
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
        let became_hot = entry_func.record_call_with_threshold(otter_vm_exec::jit_hot_threshold());
        if became_hot {
            {
                if otter_vm_exec::is_jit_enabled() {
                    otter_vm_exec::enqueue_hot_function(&module, module.entry_point, entry_func);
                    otter_vm_exec::compile_one_pending_request(
                        crate::jit_runtime::runtime_helpers(),
                    );
                }
            }
        }

        // Top-level scripts should have globalThis as `this`.
        ctx.set_pending_this(Value::object(ctx.global()));

        // Push initial frame with module reference
        ctx.register_module(&module);
        ctx.push_frame(
            module.entry_point,
            module.module_id,
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
                Err(panic_payload) => Err(VmError::internal(panic_message(&panic_payload))),
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
                                && let Ok(val) = ctx.get_local(idx as u16)
                            {
                                exports.insert(exported.clone(), val);
                            }
                        }
                        otter_vm_bytecode::module::ExportRecord::Default { local } => {
                            if let Some(idx) =
                                entry_func.local_names.iter().position(|n| n == local)
                                && let Ok(val) = ctx.get_local(idx as u16)
                            {
                                exports.insert("default".to_string(), val);
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
        ctx.pop_frame_discard();

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
            None => return VmExecutionResult::Error(VmError::internal("no entry function")),
        };
        // Record the function call for hot function detection
        let became_hot = entry_func.record_call_with_threshold(otter_vm_exec::jit_hot_threshold());
        if became_hot {
            {
                if otter_vm_exec::is_jit_enabled() {
                    otter_vm_exec::enqueue_hot_function(&module, module.entry_point, entry_func);
                    otter_vm_exec::compile_one_pending_request(
                        crate::jit_runtime::runtime_helpers(),
                    );
                }
            }
        }

        // Top-level scripts should have globalThis as `this`.
        ctx.set_pending_this(Value::object(ctx.global()));

        // Push initial frame with module reference
        ctx.register_module(&module);
        if let Err(e) = ctx.push_frame(
            module.entry_point,
            module.module_id,
            entry_func.local_count,
            None,
            false,
            entry_func.is_async(),
            0,
        ) {
            return VmExecutionResult::Error(e);
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
                    VmExecutionResult::Error(VmError::internal(panic_message(&panic_payload)))
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
        // Restore the call stack + registers from saved state (zero-copy move)
        let result_promise = async_ctx.result_promise;
        let resume_register = async_ctx.resume_register;
        let was_running = async_ctx.was_running;
        if let Err(e) = ctx.restore_frames(async_ctx.frames, async_ctx.registers) {
            return VmExecutionResult::Error(e);
        }

        // Set the resolved value in the resume register
        ctx.set_register(resume_register, resolved_value);
        ctx.set_running(was_running);

        // Continue execution with panic protection
        {
            use std::panic::{AssertUnwindSafe, catch_unwind};
            match catch_unwind(AssertUnwindSafe(|| {
                self.run_loop_with_suspension(ctx, result_promise)
            })) {
                Ok(result) => result,
                Err(panic_payload) => {
                    ctx.set_running(false);
                    VmExecutionResult::Error(VmError::internal(panic_message(&panic_payload)))
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
        // Restore the call stack + registers from saved state (zero-copy move)
        let result_promise = async_ctx.result_promise;
        let was_running = async_ctx.was_running;
        if let Err(e) = ctx.restore_frames(async_ctx.frames, async_ctx.registers) {
            return VmExecutionResult::Error(e);
        }

        ctx.set_running(was_running);

        // Set the pending throw value — the run loop will handle it
        // through try-catch or propagate it as an uncaught exception
        ctx.set_pending_throw(Some(rejection_value));

        // Continue execution with panic protection
        {
            use std::panic::{AssertUnwindSafe, catch_unwind};
            match catch_unwind(AssertUnwindSafe(|| {
                self.run_loop_with_suspension(ctx, result_promise)
            })) {
                Ok(result) => result,
                Err(panic_payload) => {
                    ctx.set_running(false);
                    VmExecutionResult::Error(VmError::internal(panic_message(&panic_payload)))
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
            let realm_id = self.realm_id_for_function(ctx, func);
            return self.call_native_fn_with_realm(
                ctx,
                native_fn,
                &this_value,
                args,
                Some(realm_id),
                false,
            );
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
            generator.set_callee_value(*func);
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

        // Record call hotness for direct/native->JS closure calls too.
        // Without this, closures invoked via JIT helpers stay interpreter-only.
        let became_hot = func_info.record_call_with_threshold(otter_vm_exec::jit_hot_threshold());
        if became_hot && otter_vm_exec::is_jit_enabled() {
            otter_vm_exec::enqueue_hot_function(&closure.module, closure.function_index, func_info);
            otter_vm_exec::compile_one_pending_request(crate::jit_runtime::runtime_helpers());
        }

        let can_try_jit = Self::can_jit(
            func_info,
            false, // not construct
            closure.is_async || closure.is_generator,
            args.len() as u8,
        );
        if can_try_jit {
            ctx.set_pending_this(this_value);
            if let Some(ref home_obj) = closure.home_object {
                ctx.set_pending_home_object(*home_obj);
            }
            ctx.set_pending_callee_value(*func);
            let jit_interp: *const Self = self;
            let jit_ctx_ptr: *mut crate::context::VmContext = ctx;
            match crate::jit_runtime::try_execute_jit(
                closure.module.module_id,
                closure.function_index,
                func_info,
                args,
                ctx.cached_proto_epoch,
                jit_interp,
                jit_ctx_ptr,
                &closure.module.constants as *const _,
                &closure.upvalues,
                None,
            ) {
                crate::jit_runtime::JitCallResult::Ok(value) => {
                    return Ok(value);
                }
                crate::jit_runtime::JitCallResult::NeedsRecompilation => {
                    otter_vm_exec::enqueue_hot_function(
                        &closure.module,
                        closure.function_index,
                        func_info,
                    );
                    otter_vm_exec::compile_one_pending_request(
                        crate::jit_runtime::runtime_helpers(),
                    );
                }
                crate::jit_runtime::JitCallResult::BailoutResume(_)
                | crate::jit_runtime::JitCallResult::BailoutRestart
                | crate::jit_runtime::JitCallResult::NotCompiled => {}
            }
        }

        // Set up the call — handle rest parameters
        let mut call_args: SmallVec<[Value; 8]> = SmallVec::from_slice(args);
        if func_info.flags.has_rest {
            let param_count = func_info.param_count as usize;
            let rest_args: Vec<Value> = if call_args.len() > param_count {
                call_args.drain(param_count..).collect()
            } else {
                Vec::new()
            };
            let rest_arr = crate::gc::GcRef::new(crate::object::JsObject::array(rest_args.len()));
            if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                && let Some(array_proto) = array_obj
                    .get(&crate::object::PropertyKey::string("prototype"))
                    .and_then(|v| v.as_object())
            {
                rest_arr.set_prototype(Value::object(array_proto));
            }
            for (i, arg) in rest_args.into_iter().enumerate() {
                let _ = rest_arr.set(crate::object::PropertyKey::Index(i as u32), arg);
            }
            call_args.push(Value::object(rest_arr));
        }

        let argc = call_args.len() as u16;
        ctx.set_pending_args(call_args);
        ctx.set_pending_this(this_value);
        ctx.set_pending_upvalues(closure.upvalues.clone());
        // Propagate home_object from closure to the new call frame
        if let Some(ref ho) = closure.home_object {
            ctx.set_pending_home_object(*ho);
        }

        let realm_id = self.realm_id_for_function(ctx, func);
        ctx.set_pending_realm_id(realm_id);
        // Store callee value for arguments.callee
        ctx.set_pending_callee_value(*func);
        ctx.register_module(&closure.module);
        ctx.push_frame(
            closure.function_index,
            closure.module.module_id,
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

            let current_module = Arc::clone(ctx.module_table.get(frame.module_id));
            let func = match current_module.function(frame.function_index) {
                Some(f) => f,
                None => return Err(VmError::internal("function not found")),
            };

            // Check if we've reached the end of the function
            if frame.pc >= func.instructions.read().len() {
                // Check if we've returned to the original depth
                if ctx.stack_depth() <= prev_stack_depth {
                    break Value::undefined();
                }
                ctx.pop_frame_discard();
                continue;
            }

            let instruction = &func.instructions.read()[frame.pc];

            match self.execute_instruction(instruction, &current_module, ctx) {
                Ok(()) => {}
                Err(err) => match err {
                    VmError::Exception(thrown) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(thrown.value));
                    }
                    VmError::TypeError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "TypeError",
                            &message,
                        )));
                    }
                    VmError::RangeError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "RangeError",
                            &message,
                        )));
                    }
                    VmError::ReferenceError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "ReferenceError",
                            &message,
                        )));
                    }
                    VmError::SyntaxError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "SyntaxError",
                            &message,
                        )));
                    }
                    VmError::URIError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(
                            self.make_error(ctx, "URIError", &message),
                        ));
                    }
                    other => {
                        while ctx.stack_depth() > prev_stack_depth {
                            ctx.pop_frame_discard();
                        }
                        ctx.set_running(was_running);
                        return Err(other);
                    }
                },
            }

            if let Some(action) = ctx.take_dispatch_action() {
                match action {
                    DispatchAction::Jump(offset) => {
                        if offset < 0 {
                            let target_pc = (ctx.current_frame().map(|f| f.pc).unwrap_or(0) as i64
                                + offset as i64)
                                as usize;
                            match self.try_back_edge_osr(ctx, &current_module, func, target_pc) {
                                BackEdgeOsrOutcome::Returned(osr_value) => {
                                    let return_reg = ctx
                                        .current_frame()
                                        .ok_or_else(|| VmError::internal("no frame"))?
                                        .return_register;
                                    if ctx.stack_depth() <= prev_stack_depth + 1 {
                                        ctx.pop_frame_discard();
                                        break osr_value;
                                    }
                                    ctx.pop_frame_discard();
                                    if let Some(reg) = return_reg {
                                        ctx.set_register(reg, osr_value);
                                    } else {
                                        ctx.set_register(0, osr_value);
                                    }
                                    continue;
                                }
                                BackEdgeOsrOutcome::ContinueAtDeoptPc => continue,
                                BackEdgeOsrOutcome::ContinueWithJump => {}
                            }
                        }
                        ctx.jump(offset);
                    }
                    DispatchAction::Return(value) => {
                        let (return_reg, is_construct, construct_this, is_async) = {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            (
                                frame.return_register,
                                frame.flags.is_construct(),
                                frame.this_value,
                                frame.flags.is_async(),
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
                            ctx.pop_frame_discard();
                            break value;
                        }
                        // Handle return from nested call
                        ctx.pop_frame_discard();
                        if let Some(reg) = return_reg {
                            ctx.set_register(reg, value);
                        } else {
                            ctx.set_register(0, value);
                        }
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
                        // Extract func info from module table (borrow scoped, no Arc clone)
                        let (local_count, has_rest, param_count, became_hot, can_try_jit) = {
                            let m = ctx.module_table.get(module_id);
                            let f = m
                                .function(func_index)
                                .ok_or_else(|| VmError::internal("function not found"))?;
                            let hot =
                                f.record_call_with_threshold(otter_vm_exec::jit_hot_threshold());
                            let jit = Self::can_jit(f, is_construct, is_async, argc);
                            (
                                f.local_count,
                                f.flags.has_rest,
                                f.param_count as usize,
                                hot,
                                jit,
                            )
                        };

                        // JIT paths (cold) — clone Arc only when needed
                        if became_hot && otter_vm_exec::is_jit_enabled() {
                            let m = Arc::clone(ctx.module_table.get(module_id));
                            let f = m.function(func_index).unwrap();
                            otter_vm_exec::enqueue_hot_function(&m, func_index, f);
                            otter_vm_exec::compile_one_pending_request(
                                crate::jit_runtime::runtime_helpers(),
                            );
                        }
                        if can_try_jit {
                            let m = Arc::clone(ctx.module_table.get(module_id));
                            let f = m.function(func_index).unwrap();
                            let jit_interp: *const Self = self;
                            let jit_ctx_ptr: *mut crate::context::VmContext = ctx;
                            match crate::jit_runtime::try_execute_jit(
                                module_id,
                                func_index,
                                f,
                                ctx.pending_args(),
                                ctx.cached_proto_epoch,
                                jit_interp,
                                jit_ctx_ptr,
                                &m.constants as *const _,
                                &upvalues,
                                None,
                            ) {
                                crate::jit_runtime::JitCallResult::Ok(value) => {
                                    ctx.set_register(return_reg, value);
                                    continue;
                                }
                                crate::jit_runtime::JitCallResult::BailoutResume(state) => {
                                    ctx.set_pending_upvalues(upvalues);
                                    ctx.push_frame(
                                        func_index,
                                        module_id,
                                        local_count,
                                        Some(return_reg),
                                        is_construct,
                                        is_async,
                                        argc as u16,
                                    )?;
                                    crate::jit_resume::resume_in_place(ctx, &state);
                                    continue;
                                }
                                crate::jit_runtime::JitCallResult::NeedsRecompilation => {
                                    otter_vm_exec::enqueue_hot_function(&m, func_index, f);
                                    otter_vm_exec::compile_one_pending_request(
                                        crate::jit_runtime::runtime_helpers(),
                                    );
                                }
                                crate::jit_runtime::JitCallResult::BailoutRestart
                                | crate::jit_runtime::JitCallResult::NotCompiled => {}
                            }
                        }

                        if has_rest {
                            let mut args = ctx.take_pending_args();
                            let rest_args: Vec<Value> = if args.len() > param_count {
                                args.drain(param_count..).collect()
                            } else {
                                Vec::new()
                            };
                            let rest_arr = GcRef::new(JsObject::array(rest_args.len()));
                            if let Some(array_obj) =
                                ctx.get_global("Array").and_then(|v| v.as_object())
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

                        // Hot path: push frame (no Arc clone)
                        ctx.set_pending_upvalues(upvalues);
                        ctx.push_frame(
                            func_index,
                            module_id,
                            local_count,
                            Some(return_reg),
                            is_construct,
                            is_async,
                            argc as u16,
                        )?;
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
                        let local_count = {
                            let m = ctx.module_table.get(module_id);
                            m.function(func_index)
                                .ok_or_else(|| VmError::internal("function not found"))?
                                .local_count
                        };
                        ctx.set_pending_upvalues(upvalues);
                        ctx.push_frame(
                            func_index,
                            module_id,
                            local_count,
                            Some(return_reg),
                            false,
                            is_async,
                            argc as u16,
                        )?;
                    }
                    DispatchAction::Suspend { .. } => {
                        // Can't handle suspension in direct call, return undefined
                        break Value::undefined();
                    }
                    DispatchAction::Yield { .. } => {
                        // Can't handle yield in direct call, return undefined
                        break Value::undefined();
                    }
                    DispatchAction::Throw(error) => {
                        // Handle throws caught inside the function(s) started by this call.
                        if let Some((target_depth, catch_pc)) = ctx.peek_nearest_try()
                            && target_depth > prev_stack_depth
                        {
                            let _ = ctx.take_nearest_try();
                            while ctx.stack_depth() > target_depth {
                                ctx.pop_frame_discard();
                            }
                            if let Some(frame) = ctx.current_frame_mut() {
                                frame.pc = catch_pc;
                            }
                            ctx.set_exception(error);
                            continue;
                        }

                        // Check if we're unwinding through an async function frame.
                        // If so, convert the error to a rejected promise instead of propagating.
                        let is_async_frame = ctx
                            .current_frame()
                            .map(|f| f.flags.is_async())
                            .unwrap_or(false);

                        // Pop the frame we pushed and unwind to original depth
                        while ctx.stack_depth() > prev_stack_depth {
                            ctx.pop_frame_discard();
                        }

                        if is_async_frame {
                            // Async function: wrap error in rejected promise
                            let rejected = self.create_js_promise(ctx, JsPromise::rejected(error));
                            ctx.set_running(was_running);
                            return Ok(rejected);
                        }

                        ctx.set_running(was_running);
                        return Err(VmError::exception(error));
                    }
                }
            } else {
                ctx.advance_pc();
            }
        };

        ctx.set_running(was_running);
        Ok(result)
    }

    /// Capture the current VM state as an AsyncContext for suspension.
    /// Moves registers + call stack out of VmContext (zero-copy).
    fn capture_async_context(
        &self,
        ctx: &mut VmContext,
        resume_register: u16,
        awaited_promise: GcRef<JsPromise>,
        result_promise: GcRef<JsPromise>,
    ) -> AsyncContext {
        let was_running = ctx.is_running();
        let (frames, registers) = ctx.take_frames();
        AsyncContext::new(
            frames,
            registers,
            result_promise,
            awaited_promise,
            resume_register,
            was_running,
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
        let mut cached_frame_id: u32 = u32::MAX;

        // Hoist trace config: trace_state doesn't change mid-execution
        let tracing_enabled = ctx.trace_state.is_some();
        let trace_capture_timing = ctx
            .trace_state
            .as_ref()
            .map(|s| s.config.capture_timing)
            .unwrap_or(false);

        // Check for pending throw (injected by resume_async_throw)
        if let Some(throw_value) = ctx.take_pending_throw() {
            // Process through try-catch machinery
            if let Some(handler) = ctx.take_nearest_try() {
                while ctx.stack_depth() > handler.0 {
                    ctx.pop_frame_discard();
                }
                if let Some(frame) = ctx.current_frame_mut() {
                    frame.pc = handler.1;
                }
                ctx.set_register(0, throw_value);
                // Fall through to the main loop
            } else {
                // No try-catch — uncaught exception
                ctx.set_running(false);
                return VmExecutionResult::Error(VmError::exception(throw_value));
            }
        }

        loop {
            // Refresh cached proto epoch once per iteration (avoids atomic load per IC access)
            ctx.cached_proto_epoch = get_proto_epoch();

            // Periodic interrupt check for responsive timeouts
            if ctx.should_check_interrupt() {
                ctx.update_debug_snapshot();
                if ctx.is_interrupted() {
                    ctx.set_running(false);
                    return VmExecutionResult::Error(VmError::Interrupted);
                }
                // Check for GC trigger at safepoint
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
                None => return VmExecutionResult::Error(VmError::internal("no frame")),
            };

            // Only clone Arc when frame changes (avoids atomic ops on hot path)
            if frame.frame_id != cached_frame_id {
                cached_module = Some(Arc::clone(ctx.module_table.get(frame.module_id)));
                cached_frame_id = frame.frame_id;
            }

            // Get reference to cached module (avoids clone on hot path)
            let module_ref = cached_module.as_ref().unwrap();
            let func = match module_ref.function(frame.function_index) {
                Some(f) => f,
                None => return VmExecutionResult::Error(VmError::internal("function not found")),
            };

            // Check if we've reached the end of the function
            if frame.pc >= func.instructions.read().len() {
                // Implicit return undefined
                if ctx.stack_depth() == 1 {
                    ctx.set_running(false);
                    return VmExecutionResult::Complete(Value::undefined());
                }
                ctx.pop_frame_discard();
                // Invalidate cache since frame changed
                cached_frame_id = u32::MAX;
                continue;
            }

            // Cache instructions reference once
            let instructions = func.instructions.read();
            let instruction = &instructions[frame.pc];

            // Record instruction execution for profiling
            ctx.record_instruction();

            // ── Inline fast paths (same as run_loop) ──
            match instruction {
                Instruction::GetLocal { dst, idx } => {
                    ctx.load_local_into_register(dst.0, idx.0);
                    if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::SetLocal { idx, src } => {
                    ctx.store_register_into_local(idx.0, src.0);
                    if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::LoadInt8 { dst, value } => {
                    ctx.set_register(dst.0, Value::int32(*value as i32));
                    if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::LoadInt32 { dst, value } => {
                    ctx.set_register(dst.0, Value::int32(*value));
                    if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::AddInt32 { dst, lhs, rhs, .. } => {
                    if let (Some(l), Some(r)) = (
                        ctx.get_register(lhs.0).as_int32(),
                        ctx.get_register(rhs.0).as_int32(),
                    ) && let Some(result) = l.checked_add(r)
                    {
                        ctx.set_register(dst.0, Value::int32(result));
                        if let Some(f) = ctx.call_stack.last_mut() {
                            f.pc += 1;
                        }
                        continue;
                    }
                }
                Instruction::SubInt32 { dst, lhs, rhs, .. } => {
                    if let (Some(l), Some(r)) = (
                        ctx.get_register(lhs.0).as_int32(),
                        ctx.get_register(rhs.0).as_int32(),
                    ) && let Some(result) = l.checked_sub(r)
                    {
                        ctx.set_register(dst.0, Value::int32(result));
                        if let Some(f) = ctx.call_stack.last_mut() {
                            f.pc += 1;
                        }
                        continue;
                    }
                }
                Instruction::MulInt32 { dst, lhs, rhs, .. } => {
                    if let (Some(l), Some(r)) = (
                        ctx.get_register(lhs.0).as_int32(),
                        ctx.get_register(rhs.0).as_int32(),
                    ) && let Some(result) = l.checked_mul(r)
                    {
                        ctx.set_register(dst.0, Value::int32(result));
                        if let Some(f) = ctx.call_stack.last_mut() {
                            f.pc += 1;
                        }
                        continue;
                    }
                }
                Instruction::Jump { offset } if offset.0 > 0 => {
                    ctx.jump(offset.0);
                    continue;
                }
                Instruction::JumpIfTrue { cond, offset } if offset.0 >= 0 => {
                    if ctx.get_register(cond.0).to_boolean() {
                        ctx.jump(offset.0);
                    } else if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::JumpIfFalse { cond, offset } if offset.0 >= 0 => {
                    if !ctx.get_register(cond.0).to_boolean() {
                        ctx.jump(offset.0);
                    } else if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::GetLocal2 {
                    dst1,
                    idx1,
                    dst2,
                    idx2,
                } => {
                    ctx.load_local_into_register(dst1.0, idx1.0);
                    ctx.load_local_into_register(dst2.0, idx2.0);
                    if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::IncLocal { local_idx, src } => {
                    let val = ctx.get_register(src.0);
                    if let Some(i) = val.as_int32()
                        && let Some(result) = i.checked_add(1)
                    {
                        let v = Value::int32(result);
                        ctx.set_register(src.0, v);
                        ctx.store_register_into_local(local_idx.0, src.0);
                        if let Some(f) = ctx.call_stack.last_mut() {
                            f.pc += 1;
                        }
                        continue;
                    }
                }
                Instruction::Return { src } => {
                    let (is_derived, return_reg, is_construct, is_async, construct_this) = {
                        let frame = ctx.current_frame().expect("no frame");
                        (
                            frame.flags.is_derived(),
                            frame.return_register,
                            frame.flags.is_construct(),
                            frame.flags.is_async(),
                            if frame.flags.is_construct() {
                                frame.this_value
                            } else {
                                Value::undefined()
                            },
                        )
                    };
                    if !is_derived {
                        let value = *ctx.get_register(src.0);
                        if ctx.stack_depth() == 1 {
                            return VmExecutionResult::Complete(value);
                        }
                        ctx.pop_frame_discard();
                        cached_frame_id = u32::MAX;
                        if let Some(reg) = return_reg {
                            let result = if is_construct && !value.is_object() {
                                construct_this
                            } else if is_async {
                                self.create_js_promise(
                                    ctx,
                                    crate::promise::JsPromise::resolved(value),
                                )
                            } else {
                                value
                            };
                            ctx.set_register(reg, result);
                        }
                        continue;
                    }
                }
                Instruction::ReturnUndefined => {
                    let (is_derived, return_reg, is_construct, is_async, construct_this) = {
                        let frame = ctx.current_frame().expect("no frame");
                        (
                            frame.flags.is_derived(),
                            frame.return_register,
                            frame.flags.is_construct(),
                            frame.flags.is_async(),
                            if frame.flags.is_construct() {
                                frame.this_value
                            } else {
                                Value::undefined()
                            },
                        )
                    };
                    if !is_derived {
                        if ctx.stack_depth() == 1 {
                            return VmExecutionResult::Complete(Value::undefined());
                        }
                        ctx.pop_frame_discard();
                        cached_frame_id = u32::MAX;
                        if let Some(reg) = return_reg {
                            let result = if is_construct {
                                construct_this
                            } else if is_async {
                                self.create_js_promise(
                                    ctx,
                                    crate::promise::JsPromise::resolved(Value::undefined()),
                                )
                            } else {
                                Value::undefined()
                            };
                            ctx.set_register(reg, result);
                        }
                        continue;
                    }
                }
                _ => {} // Fall through to full dispatch
            }

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

            // Execute the instruction
            match self.execute_instruction(instruction, module_ref, ctx) {
                Ok(()) => {}
                Err(err) => match err {
                    VmError::Exception(thrown) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(thrown.value));
                    }
                    VmError::TypeError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "TypeError",
                            &message,
                        )));
                    }
                    VmError::RangeError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "RangeError",
                            &message,
                        )));
                    }
                    VmError::ReferenceError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "ReferenceError",
                            &message,
                        )));
                    }
                    VmError::SyntaxError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "SyntaxError",
                            &message,
                        )));
                    }
                    VmError::URIError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(
                            self.make_error(ctx, "URIError", &message),
                        ));
                    }
                    other => return VmExecutionResult::Error(other),
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
                                + offset as i64)
                                as usize;
                            match self.try_back_edge_osr(ctx, module_ref, func, target_pc) {
                                BackEdgeOsrOutcome::Returned(osr_value) => {
                                    if ctx.stack_depth() == 1 {
                                        ctx.set_running(false);
                                        return VmExecutionResult::Complete(osr_value);
                                    }
                                    let return_reg = ctx
                                        .current_frame()
                                        .map(|f| f.return_register)
                                        .unwrap_or(None);
                                    ctx.pop_frame_discard();
                                    cached_frame_id = u32::MAX;
                                    if let Some(reg) = return_reg {
                                        ctx.set_register(reg, osr_value);
                                    }
                                    continue;
                                }
                                BackEdgeOsrOutcome::ContinueAtDeoptPc => continue,
                                BackEdgeOsrOutcome::ContinueWithJump => {}
                            }
                        }
                        ctx.jump(offset);
                    }
                    DispatchAction::Return(value) => {
                        if ctx.stack_depth() == 1 {
                            ctx.set_running(false);
                            return VmExecutionResult::Complete(value);
                        }

                        let (return_reg, is_construct, construct_this, is_async) = {
                            let frame = match ctx.current_frame() {
                                Some(f) => f,
                                None => {
                                    return VmExecutionResult::Error(VmError::internal("no frame"));
                                }
                            };
                            (
                                frame.return_register,
                                frame.flags.is_construct(),
                                frame.this_value,
                                frame.flags.is_async(),
                            )
                        };
                        ctx.pop_frame_discard();
                        // Invalidate cache since frame changed
                        cached_frame_id = u32::MAX;

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
                    DispatchAction::Throw(value) => {
                        // Unwind to nearest try handler if present
                        if let Some((target_depth, catch_pc)) = ctx.take_nearest_try() {
                            // Pop frames above the handler
                            while ctx.stack_depth() > target_depth {
                                ctx.pop_frame_discard();
                            }
                            // Invalidate cache since frames changed
                            cached_frame_id = u32::MAX;

                            // Jump to catch block in the handler frame
                            let frame = match ctx.current_frame_mut() {
                                Some(f) => f,
                                None => {
                                    return VmExecutionResult::Error(VmError::internal("no frame"));
                                }
                            };
                            frame.pc = catch_pc;

                            ctx.set_exception(value);
                            continue;
                        }

                        // Check if the current frame is async — if so, convert the
                        // error to a rejected promise and continue in the caller.
                        let is_async = ctx
                            .current_frame()
                            .map(|f| f.flags.is_async())
                            .unwrap_or(false);

                        if is_async && ctx.stack_depth() > 1 {
                            let return_reg = ctx.current_frame().and_then(|f| f.return_register);
                            ctx.pop_frame_discard();
                            cached_frame_id = u32::MAX;
                            let rejected = self.create_js_promise(ctx, JsPromise::rejected(value));
                            if let Some(reg) = return_reg {
                                ctx.set_register(reg, rejected);
                            }
                            continue;
                        }

                        // No handler: return as error
                        ctx.set_running(false);
                        return VmExecutionResult::Error(VmError::exception(value));
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
                        // Extract func info with scoped borrow (no Arc clone on hot path)
                        let (local_count, has_rest, param_count, became_hot, can_try_jit) = {
                            let m = ctx.module_table.get(module_id);
                            let f = match m.function(func_index) {
                                Some(f) => f,
                                None => {
                                    return VmExecutionResult::Error(VmError::internal(format!(
                                        "callee not found (func_index={}, function_count={})",
                                        func_index,
                                        m.function_count()
                                    )));
                                }
                            };
                            let hot =
                                f.record_call_with_threshold(otter_vm_exec::jit_hot_threshold());
                            let jit = Self::can_jit(f, is_construct, is_async, argc);
                            (
                                f.local_count,
                                f.flags.has_rest,
                                f.param_count as usize,
                                hot,
                                jit,
                            )
                        };

                        // JIT paths (cold) — clone Arc only when needed
                        if became_hot && otter_vm_exec::is_jit_enabled() {
                            let m = Arc::clone(ctx.module_table.get(module_id));
                            let f = m.function(func_index).unwrap();
                            otter_vm_exec::enqueue_hot_function(&m, func_index, f);
                            otter_vm_exec::compile_one_pending_request(
                                crate::jit_runtime::runtime_helpers(),
                            );
                        }
                        if can_try_jit {
                            let m = Arc::clone(ctx.module_table.get(module_id));
                            let f = m.function(func_index).unwrap();
                            let jit_interp: *const Self = self;
                            let jit_ctx_ptr: *mut crate::context::VmContext = ctx;
                            match crate::jit_runtime::try_execute_jit(
                                module_id,
                                func_index,
                                f,
                                ctx.pending_args(),
                                ctx.cached_proto_epoch,
                                jit_interp,
                                jit_ctx_ptr,
                                &m.constants as *const _,
                                &upvalues,
                                None,
                            ) {
                                crate::jit_runtime::JitCallResult::Ok(value) => {
                                    ctx.set_register(return_reg, value);
                                    continue;
                                }
                                crate::jit_runtime::JitCallResult::BailoutResume(state) => {
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
                                        return VmExecutionResult::Error(e);
                                    }
                                    crate::jit_resume::resume_in_place(ctx, &state);
                                    continue;
                                }
                                crate::jit_runtime::JitCallResult::NeedsRecompilation => {
                                    otter_vm_exec::enqueue_hot_function(&m, func_index, f);
                                    otter_vm_exec::compile_one_pending_request(
                                        crate::jit_runtime::runtime_helpers(),
                                    );
                                }
                                crate::jit_runtime::JitCallResult::BailoutRestart
                                | crate::jit_runtime::JitCallResult::NotCompiled => {}
                            }
                        }

                        // Handle rest parameters
                        if has_rest {
                            let mut args = ctx.take_pending_args();
                            let rest_args: Vec<Value> = if args.len() > param_count {
                                args.drain(param_count..).collect()
                            } else {
                                Vec::new()
                            };
                            let rest_arr = GcRef::new(JsObject::array(rest_args.len()));
                            if let Some(array_obj) =
                                ctx.get_global("Array").and_then(|v| v.as_object())
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

                        // Hot path: push frame (no Arc clone)
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
                            return VmExecutionResult::Error(e);
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
                            let f = match m.function(func_index) {
                                Some(f) => f,
                                None => {
                                    return VmExecutionResult::Error(VmError::internal(format!(
                                        "callee not found (func_index={}, function_count={})",
                                        func_index,
                                        m.function_count()
                                    )));
                                }
                            };
                            (f.local_count, f.flags.has_rest, f.param_count as usize)
                        };

                        if has_rest {
                            let mut args = ctx.take_pending_args();
                            let rest_args: Vec<Value> = if args.len() > param_count {
                                args.drain(param_count..).collect()
                            } else {
                                Vec::new()
                            };
                            let rest_arr = GcRef::new(JsObject::array(rest_args.len()));
                            if let Some(array_obj) =
                                ctx.get_global("Array").and_then(|v| v.as_object())
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
                            return VmExecutionResult::Error(e);
                        }
                    }
                    DispatchAction::Suspend {
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
                                return VmExecutionResult::Error(VmError::exception(error));
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
                    DispatchAction::Yield { value, .. } => {
                        // Generator yielded a value
                        let result = GcRef::new(JsObject::new(Value::null()));
                        let _ = result.set(PropertyKey::string("value"), value);
                        let _ = result.set(PropertyKey::string("done"), Value::boolean(false));
                        ctx.advance_pc();
                        return VmExecutionResult::Complete(Value::object(result));
                    }
                }
            } else {
                ctx.advance_pc();
            }
        }
    }

    pub fn serialize(&self, _memory_manager: &MemoryManager) -> VmResult<Vec<u8>> {
        // This function is a placeholder and should be implemented to serialize the VM state.
        unimplemented!("Serialization is not yet implemented for the VM");
    }

    /// Main execution loop
    fn run_loop(&self, ctx: &mut VmContext) -> VmResult<Value> {
        // If tracing is enabled, use the traced variant to keep the hot path clean
        if ctx.trace_state.is_some() {
            return self.run_loop_traced(ctx);
        }

        // Cache module Arc - only refresh when frame changes
        let mut cached_module: Option<Arc<Module>> = None;
        let mut cached_frame_id: u32 = u32::MAX;

        loop {
            // Periodic interrupt check (batched every INTERRUPT_CHECK_INTERVAL instructions)
            if ctx.should_check_interrupt() {
                // Refresh proto epoch only at interrupt boundaries (not every instruction)
                ctx.cached_proto_epoch = get_proto_epoch();
                ctx.update_debug_snapshot();
                if ctx.is_interrupted() {
                    return Err(VmError::interrupted());
                }
                // Check for GC trigger at safepoint
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

            // SAFETY: call_stack is never empty during execution (checked at entry)
            let frame = match ctx.call_stack.last() {
                Some(f) => f,
                None => return Err(VmError::internal("no frame")),
            };

            // Only clone Arc when frame changes (avoids atomic ops on hot path)
            if frame.frame_id != cached_frame_id {
                cached_module = Some(Arc::clone(ctx.module_table.get(frame.module_id)));
                cached_frame_id = frame.frame_id;
            }

            // Get reference to cached module (avoids clone on hot path for func lookup)
            let module_ref = cached_module.as_ref().unwrap();
            let func = match module_ref.function(frame.function_index) {
                Some(f) => f,
                None => return Err(VmError::internal("function not found")),
            };

            // Cache instructions reference once (avoid double .read() call)
            let instructions = func.instructions.read();

            // Check if we've reached the end of the function
            let pc = frame.pc;
            if pc >= instructions.len() {
                // Implicit return undefined
                if ctx.stack_depth() == 1 {
                    return Ok(Value::undefined());
                }
                ctx.pop_frame_discard();
                // Invalidate cache since frame changed
                cached_frame_id = u32::MAX;
                continue;
            }

            let instruction = &instructions[pc];

            // Record instruction execution for profiling
            ctx.record_instruction();

            // ── Inline fast paths for hottest instructions ──
            // These bypass execute_instruction entirely, avoiding the function call,
            // the 150+ arm match dispatch, and DispatchAction enum construction.
            match instruction {
                Instruction::GetLocal { dst, idx } => {
                    ctx.load_local_into_register(dst.0, idx.0);
                    if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::SetLocal { idx, src } => {
                    ctx.store_register_into_local(idx.0, src.0);
                    if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::LoadInt8 { dst, value } => {
                    ctx.set_register(dst.0, Value::int32(*value as i32));
                    if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::LoadInt32 { dst, value } => {
                    ctx.set_register(dst.0, Value::int32(*value));
                    if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::AddInt32 { dst, lhs, rhs, .. } => {
                    if let (Some(l), Some(r)) = (
                        ctx.get_register(lhs.0).as_int32(),
                        ctx.get_register(rhs.0).as_int32(),
                    ) && let Some(result) = l.checked_add(r)
                    {
                        ctx.set_register(dst.0, Value::int32(result));
                        if let Some(f) = ctx.call_stack.last_mut() {
                            f.pc += 1;
                        }
                        continue;
                    }
                    // Fall through to execute_instruction for de-quicken path
                }
                Instruction::SubInt32 { dst, lhs, rhs, .. } => {
                    if let (Some(l), Some(r)) = (
                        ctx.get_register(lhs.0).as_int32(),
                        ctx.get_register(rhs.0).as_int32(),
                    ) && let Some(result) = l.checked_sub(r)
                    {
                        ctx.set_register(dst.0, Value::int32(result));
                        if let Some(f) = ctx.call_stack.last_mut() {
                            f.pc += 1;
                        }
                        continue;
                    }
                    // Fall through to execute_instruction for de-quicken path
                }
                Instruction::MulInt32 { dst, lhs, rhs, .. } => {
                    if let (Some(l), Some(r)) = (
                        ctx.get_register(lhs.0).as_int32(),
                        ctx.get_register(rhs.0).as_int32(),
                    ) && let Some(result) = l.checked_mul(r)
                    {
                        ctx.set_register(dst.0, Value::int32(result));
                        if let Some(f) = ctx.call_stack.last_mut() {
                            f.pc += 1;
                        }
                        continue;
                    }
                    // Fall through to execute_instruction for de-quicken path
                }
                Instruction::Jump { offset } if offset.0 > 0 => {
                    // Forward jump only (backward jumps need OSR check)
                    ctx.jump(offset.0);
                    continue;
                }
                Instruction::JumpIfTrue { cond, offset } if offset.0 >= 0 => {
                    if ctx.get_register(cond.0).to_boolean() {
                        ctx.jump(offset.0);
                    } else if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::JumpIfFalse { cond, offset } if offset.0 >= 0 => {
                    if !ctx.get_register(cond.0).to_boolean() {
                        ctx.jump(offset.0);
                    } else if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::GetLocal2 {
                    dst1,
                    idx1,
                    dst2,
                    idx2,
                } => {
                    ctx.load_local_into_register(dst1.0, idx1.0);
                    ctx.load_local_into_register(dst2.0, idx2.0);
                    if let Some(f) = ctx.call_stack.last_mut() {
                        f.pc += 1;
                    }
                    continue;
                }
                Instruction::IncLocal { local_idx, src } => {
                    let val = ctx.get_register(src.0);
                    if let Some(i) = val.as_int32()
                        && let Some(result) = i.checked_add(1)
                    {
                        let v = Value::int32(result);
                        ctx.set_register(src.0, v);
                        ctx.store_register_into_local(local_idx.0, src.0);
                        if let Some(f) = ctx.call_stack.last_mut() {
                            f.pc += 1;
                        }
                        continue;
                    }
                    // Fall through for non-int32 or overflow
                }
                // Inline Return fast path: simple returns bypass execute_instruction
                // and DispatchAction round-trip. Pops frame directly.
                Instruction::Return { src } => {
                    // Extract frame metadata before mutating ctx
                    let (is_derived, return_reg, is_construct, is_async, construct_this) = {
                        let frame = ctx.current_frame().expect("no frame");
                        (
                            frame.flags.is_derived(),
                            frame.return_register,
                            frame.flags.is_construct(),
                            frame.flags.is_async(),
                            if frame.flags.is_construct() {
                                frame.this_value
                            } else {
                                Value::undefined()
                            },
                        )
                    };
                    if !is_derived {
                        let value = *ctx.get_register(src.0);
                        if ctx.stack_depth() == 1 {
                            return Ok(value);
                        }
                        ctx.pop_frame_discard();
                        cached_frame_id = u32::MAX;
                        if let Some(reg) = return_reg {
                            let result = if is_construct && !value.is_object() {
                                construct_this
                            } else if is_async {
                                self.create_js_promise(
                                    ctx,
                                    crate::promise::JsPromise::resolved(value),
                                )
                            } else {
                                value
                            };
                            ctx.set_register(reg, result);
                        }
                        continue;
                    }
                    // Fall through for derived constructors
                }
                Instruction::ReturnUndefined => {
                    let (is_derived, return_reg, is_construct, is_async, construct_this) = {
                        let frame = ctx.current_frame().expect("no frame");
                        (
                            frame.flags.is_derived(),
                            frame.return_register,
                            frame.flags.is_construct(),
                            frame.flags.is_async(),
                            if frame.flags.is_construct() {
                                frame.this_value
                            } else {
                                Value::undefined()
                            },
                        )
                    };
                    if !is_derived {
                        if ctx.stack_depth() == 1 {
                            return Ok(Value::undefined());
                        }
                        ctx.pop_frame_discard();
                        cached_frame_id = u32::MAX;
                        if let Some(reg) = return_reg {
                            let result = if is_construct {
                                construct_this
                            } else if is_async {
                                self.create_js_promise(
                                    ctx,
                                    crate::promise::JsPromise::resolved(Value::undefined()),
                                )
                            } else {
                                Value::undefined()
                            };
                            ctx.set_register(reg, result);
                        }
                        continue;
                    }
                    // Fall through for derived constructors
                }
                _ => {} // Fall through to full dispatch
            }

            // Full dispatch for all other instructions
            match self.execute_instruction(instruction, module_ref, ctx) {
                Ok(()) => {}
                Err(err) => match err {
                    VmError::Exception(thrown) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(thrown.value));
                    }
                    VmError::TypeError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "TypeError",
                            &message,
                        )));
                    }
                    VmError::RangeError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "RangeError",
                            &message,
                        )));
                    }
                    VmError::ReferenceError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "ReferenceError",
                            &message,
                        )));
                    }
                    VmError::SyntaxError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "SyntaxError",
                            &message,
                        )));
                    }
                    VmError::URIError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(
                            self.make_error(ctx, "URIError", &message),
                        ));
                    }
                    other => return Err(other),
                },
            }

            if let Some(action) = ctx.take_dispatch_action() {
                match action {
                    DispatchAction::Jump(offset) => {
                        if offset < 0 {
                            let target_pc = (ctx.current_frame().map(|f| f.pc).unwrap_or(0) as i64
                                + offset as i64)
                                as usize;
                            match self.try_back_edge_osr(ctx, module_ref, func, target_pc) {
                                BackEdgeOsrOutcome::Returned(osr_value) => {
                                    if ctx.stack_depth() == 1 {
                                        return Ok(osr_value);
                                    }
                                    let return_reg = ctx
                                        .current_frame()
                                        .ok_or_else(|| VmError::internal("no frame"))?
                                        .return_register;
                                    ctx.pop_frame_discard();
                                    cached_frame_id = u32::MAX;
                                    if let Some(reg) = return_reg {
                                        ctx.set_register(reg, osr_value);
                                    }
                                    continue;
                                }
                                BackEdgeOsrOutcome::ContinueAtDeoptPc => continue,
                                BackEdgeOsrOutcome::ContinueWithJump => {}
                            }
                        }
                        ctx.jump(offset);
                    }
                    DispatchAction::Return(value) => {
                        if ctx.stack_depth() == 1 {
                            return Ok(value);
                        }

                        let (return_reg, is_construct, construct_this, is_async) = {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            (
                                frame.return_register,
                                frame.flags.is_construct(),
                                frame.this_value,
                                frame.flags.is_async(),
                            )
                        };
                        ctx.pop_frame_discard();
                        // Invalidate cache since frame changed
                        cached_frame_id = u32::MAX;

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
                    DispatchAction::Throw(value) => {
                        // Unwind to nearest try handler if present
                        if let Some((target_depth, catch_pc)) = ctx.take_nearest_try() {
                            // Pop frames above the handler
                            while ctx.stack_depth() > target_depth {
                                ctx.pop_frame_discard();
                            }
                            // Invalidate cache since frames changed
                            cached_frame_id = u32::MAX;

                            // Jump to catch block in the handler frame
                            let frame = ctx
                                .current_frame_mut()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            frame.pc = catch_pc;

                            ctx.set_exception(value);
                            continue;
                        }

                        // Check if the current frame is async — if so, convert the
                        // error to a rejected promise and return it to the caller
                        // instead of propagating as an uncaught exception.
                        let is_async = ctx
                            .current_frame()
                            .map(|f| f.flags.is_async())
                            .unwrap_or(false);
                        let return_reg = ctx.current_frame().and_then(|f| f.return_register);

                        if is_async && ctx.stack_depth() > 1 {
                            ctx.pop_frame_discard();
                            cached_frame_id = u32::MAX;
                            let rejected = self.create_js_promise(ctx, JsPromise::rejected(value));
                            if let Some(reg) = return_reg {
                                ctx.set_register(reg, rejected);
                            }
                            continue;
                        }

                        // No handler: convert to an uncaught exception
                        return Err(VmError::Exception(Box::new(crate::error::ThrownValue {
                            message: self.to_string(&value),
                            value,
                            stack: Vec::new(),
                        })));
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
                        // Extract func info with scoped borrow (no Arc clone on hot path)
                        let (local_count, has_rest, param_count, became_hot, can_try_jit) = {
                            let m = ctx.module_table.get(module_id);
                            let f = m.function(func_index).ok_or_else(|| {
                                VmError::internal(format!(
                                    "callee not found (func_index={}, function_count={})",
                                    func_index,
                                    m.function_count()
                                ))
                            })?;
                            let hot =
                                f.record_call_with_threshold(otter_vm_exec::jit_hot_threshold());
                            let jit = Self::can_jit(f, is_construct, is_async, argc);
                            (
                                f.local_count,
                                f.flags.has_rest,
                                f.param_count as usize,
                                hot,
                                jit,
                            )
                        };

                        // JIT paths (cold) — clone Arc only when needed
                        if became_hot && otter_vm_exec::is_jit_enabled() {
                            let m = Arc::clone(ctx.module_table.get(module_id));
                            let f = m.function(func_index).unwrap();
                            otter_vm_exec::enqueue_hot_function(&m, func_index, f);
                            otter_vm_exec::compile_one_pending_request(
                                crate::jit_runtime::runtime_helpers(),
                            );
                        }
                        if can_try_jit {
                            let m = Arc::clone(ctx.module_table.get(module_id));
                            let f = m.function(func_index).unwrap();
                            let jit_interp: *const Self = self;
                            let jit_ctx_ptr: *mut crate::context::VmContext = ctx;
                            match crate::jit_runtime::try_execute_jit(
                                module_id,
                                func_index,
                                f,
                                ctx.pending_args(),
                                ctx.cached_proto_epoch,
                                jit_interp,
                                jit_ctx_ptr,
                                &m.constants as *const _,
                                &upvalues,
                                None,
                            ) {
                                crate::jit_runtime::JitCallResult::Ok(value) => {
                                    ctx.set_register(return_reg, value);
                                    continue;
                                }
                                crate::jit_runtime::JitCallResult::BailoutResume(state) => {
                                    ctx.set_pending_upvalues(upvalues);
                                    ctx.push_frame(
                                        func_index,
                                        module_id,
                                        local_count,
                                        Some(return_reg),
                                        is_construct,
                                        is_async,
                                        argc as u16,
                                    )?;
                                    crate::jit_resume::resume_in_place(ctx, &state);
                                    continue;
                                }
                                crate::jit_runtime::JitCallResult::NeedsRecompilation => {
                                    otter_vm_exec::enqueue_hot_function(&m, func_index, f);
                                    otter_vm_exec::compile_one_pending_request(
                                        crate::jit_runtime::runtime_helpers(),
                                    );
                                }
                                crate::jit_runtime::JitCallResult::BailoutRestart
                                | crate::jit_runtime::JitCallResult::NotCompiled => {}
                            }
                        }

                        // Handle rest parameters
                        if has_rest {
                            let mut args = ctx.take_pending_args();
                            let rest_args: Vec<Value> = if args.len() > param_count {
                                args.drain(param_count..).collect()
                            } else {
                                Vec::new()
                            };
                            let rest_arr = GcRef::new(JsObject::array(rest_args.len()));
                            if let Some(array_obj) =
                                ctx.get_global("Array").and_then(|v| v.as_object())
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

                        // Hot path: push frame (no Arc clone)
                        ctx.set_pending_upvalues(upvalues);
                        ctx.push_frame(
                            func_index,
                            module_id,
                            local_count,
                            Some(return_reg),
                            is_construct,
                            is_async,
                            argc as u16,
                        )?;
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
                            let f = m.function(func_index).ok_or_else(|| {
                                VmError::internal(format!(
                                    "callee not found (func_index={}, function_count={})",
                                    func_index,
                                    m.function_count()
                                ))
                            })?;
                            (f.local_count, f.flags.has_rest, f.param_count as usize)
                        };

                        if has_rest {
                            let mut args = ctx.take_pending_args();
                            let rest_args: Vec<Value> = if args.len() > param_count {
                                args.drain(param_count..).collect()
                            } else {
                                Vec::new()
                            };
                            let rest_arr = GcRef::new(JsObject::array(rest_args.len()));
                            if let Some(array_obj) =
                                ctx.get_global("Array").and_then(|v| v.as_object())
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
                            module_id,
                            local_count,
                            Some(return_reg),
                            false,
                            is_async,
                            argc as u16,
                        )?;
                    }
                    DispatchAction::Suspend {
                        promise,
                        resume_reg,
                    } => {
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
                    DispatchAction::Yield { value, .. } => {
                        // Generator yielded a value
                        // Create an iterator result object { value, done: false }
                        let result = GcRef::new(JsObject::new(Value::null()));
                        let _ = result.set(PropertyKey::string("value"), value);
                        let _ = result.set(PropertyKey::string("done"), Value::boolean(false));
                        ctx.advance_pc();
                        return Ok(Value::object(result));
                    }
                }
            } else {
                // Direct PC increment — avoid current_frame_mut() indirection
                if let Some(f) = ctx.call_stack.last_mut() {
                    f.pc += 1;
                }
            }
        }
    }

    /// Traced variant of run_loop — only used when ctx.trace_state is Some.
    /// Separated to keep the hot path (run_loop) free of tracing overhead.
    fn run_loop_traced(&self, ctx: &mut VmContext) -> VmResult<Value> {
        let mut cached_module: Option<Arc<Module>> = None;
        let mut cached_frame_id: u32 = u32::MAX;

        let trace_capture_timing = ctx
            .trace_state
            .as_ref()
            .map(|s| s.config.capture_timing)
            .unwrap_or(false);

        loop {
            ctx.cached_proto_epoch = get_proto_epoch();

            if ctx.should_check_interrupt() {
                ctx.update_debug_snapshot();
                if ctx.is_interrupted() {
                    return Err(VmError::interrupted());
                }
                ctx.maybe_collect_garbage();
                if crate::weak_gc::has_pending_cleanups() {
                    let cleanups = crate::weak_gc::drain_pending_cleanups();
                    for (callback, held_value) in cleanups {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        let _ = ncx.call_function(&callback, Value::undefined(), &[held_value]);
                    }
                }
            }

            let frame = match ctx.call_stack.last() {
                Some(f) => f,
                None => return Err(VmError::internal("no frame")),
            };

            if frame.frame_id != cached_frame_id {
                cached_module = Some(Arc::clone(ctx.module_table.get(frame.module_id)));
                cached_frame_id = frame.frame_id;
            }

            let module_ref = cached_module.as_ref().unwrap();
            let func = match module_ref.function(frame.function_index) {
                Some(f) => f,
                None => return Err(VmError::internal("function not found")),
            };

            let instructions = func.instructions.read();
            let pc = frame.pc;
            if pc >= instructions.len() {
                if ctx.stack_depth() == 1 {
                    return Ok(Value::undefined());
                }
                ctx.pop_frame_discard();
                cached_frame_id = u32::MAX;
                continue;
            }

            let instruction = &instructions[pc];
            ctx.record_instruction();

            // Capture trace data
            let trace_data = Some((
                frame.pc,
                frame.function_index,
                Arc::clone(ctx.module_table.get(frame.module_id)),
                instruction.clone(),
            ));
            let trace_start_time = if trace_capture_timing {
                Some(std::time::Instant::now())
            } else {
                None
            };

            match self.execute_instruction(instruction, module_ref, ctx) {
                Ok(()) => {}
                Err(err) => match err {
                    VmError::Exception(thrown) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(thrown.value));
                    }
                    VmError::TypeError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "TypeError",
                            &message,
                        )));
                    }
                    VmError::RangeError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "RangeError",
                            &message,
                        )));
                    }
                    VmError::ReferenceError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "ReferenceError",
                            &message,
                        )));
                    }
                    VmError::SyntaxError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(self.make_error(
                            ctx,
                            "SyntaxError",
                            &message,
                        )));
                    }
                    VmError::URIError(message) => {
                        ctx.dispatch_action = Some(DispatchAction::Throw(
                            self.make_error(ctx, "URIError", &message),
                        ));
                    }
                    other => return Err(other),
                },
            }

            // Record trace entry
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
                                + offset as i64)
                                as usize;
                            match self.try_back_edge_osr(ctx, module_ref, func, target_pc) {
                                BackEdgeOsrOutcome::Returned(osr_value) => {
                                    if ctx.stack_depth() == 1 {
                                        return Ok(osr_value);
                                    }
                                    let return_reg = ctx
                                        .current_frame()
                                        .ok_or_else(|| VmError::internal("no frame"))?
                                        .return_register;
                                    ctx.pop_frame_discard();
                                    cached_frame_id = u32::MAX;
                                    if let Some(reg) = return_reg {
                                        ctx.set_register(reg, osr_value);
                                    }
                                    continue;
                                }
                                BackEdgeOsrOutcome::ContinueAtDeoptPc => continue,
                                BackEdgeOsrOutcome::ContinueWithJump => {}
                            }
                        }
                        ctx.jump(offset);
                    }
                    DispatchAction::Return(value) => {
                        if ctx.stack_depth() == 1 {
                            return Ok(value);
                        }
                        let (return_reg, is_construct, construct_this, is_async) = {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            (
                                frame.return_register,
                                frame.flags.is_construct(),
                                frame.this_value,
                                frame.flags.is_async(),
                            )
                        };
                        ctx.pop_frame_discard();
                        cached_frame_id = u32::MAX;
                        if let Some(reg) = return_reg {
                            let value = if is_construct && !value.is_object() {
                                construct_this
                            } else if is_async {
                                self.create_js_promise(ctx, JsPromise::resolved(value))
                            } else {
                                value
                            };
                            ctx.set_register(reg, value);
                        }
                    }
                    DispatchAction::Throw(value) => {
                        if let Some((target_depth, catch_pc)) = ctx.take_nearest_try() {
                            while ctx.stack_depth() > target_depth {
                                ctx.pop_frame_discard();
                            }
                            cached_frame_id = u32::MAX;
                            let frame = ctx
                                .current_frame_mut()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            frame.pc = catch_pc;
                            ctx.set_exception(value);
                            continue;
                        }
                        let is_async = ctx
                            .current_frame()
                            .map(|f| f.flags.is_async())
                            .unwrap_or(false);
                        let return_reg = ctx.current_frame().and_then(|f| f.return_register);
                        if is_async && ctx.stack_depth() > 1 {
                            ctx.pop_frame_discard();
                            cached_frame_id = u32::MAX;
                            let rejected = self.create_js_promise(ctx, JsPromise::rejected(value));
                            if let Some(reg) = return_reg {
                                ctx.set_register(reg, rejected);
                            }
                            continue;
                        }
                        return Err(VmError::Exception(Box::new(crate::error::ThrownValue {
                            message: self.to_string(&value),
                            value,
                            stack: Vec::new(),
                        })));
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
                        // Extract func info with scoped borrow (no Arc clone on hot path)
                        let (local_count, has_rest, param_count, became_hot, can_try_jit) = {
                            let m = ctx.module_table.get(module_id);
                            let f = m.function(func_index).ok_or_else(|| {
                                VmError::internal(format!(
                                    "callee not found (func_index={}, function_count={})",
                                    func_index,
                                    m.function_count()
                                ))
                            })?;
                            let hot =
                                f.record_call_with_threshold(otter_vm_exec::jit_hot_threshold());
                            let jit = Self::can_jit(f, is_construct, is_async, argc);
                            (
                                f.local_count,
                                f.flags.has_rest,
                                f.param_count as usize,
                                hot,
                                jit,
                            )
                        };

                        // JIT paths (cold) — clone Arc only when needed
                        if became_hot && otter_vm_exec::is_jit_enabled() {
                            let m = Arc::clone(ctx.module_table.get(module_id));
                            let f = m.function(func_index).unwrap();
                            otter_vm_exec::enqueue_hot_function(&m, func_index, f);
                            otter_vm_exec::compile_one_pending_request(
                                crate::jit_runtime::runtime_helpers(),
                            );
                        }
                        if can_try_jit {
                            let m = Arc::clone(ctx.module_table.get(module_id));
                            let f = m.function(func_index).unwrap();
                            let jit_interp: *const Self = self;
                            let jit_ctx_ptr: *mut crate::context::VmContext = ctx;
                            match crate::jit_runtime::try_execute_jit(
                                module_id,
                                func_index,
                                f,
                                ctx.pending_args(),
                                ctx.cached_proto_epoch,
                                jit_interp,
                                jit_ctx_ptr,
                                &m.constants as *const _,
                                &upvalues,
                                None,
                            ) {
                                crate::jit_runtime::JitCallResult::Ok(value) => {
                                    ctx.set_register(return_reg, value);
                                    continue;
                                }
                                crate::jit_runtime::JitCallResult::BailoutResume(state) => {
                                    ctx.set_pending_upvalues(upvalues);
                                    ctx.push_frame(
                                        func_index,
                                        module_id,
                                        local_count,
                                        Some(return_reg),
                                        is_construct,
                                        is_async,
                                        argc as u16,
                                    )?;
                                    crate::jit_resume::resume_in_place(ctx, &state);
                                    continue;
                                }
                                crate::jit_runtime::JitCallResult::NeedsRecompilation => {
                                    otter_vm_exec::enqueue_hot_function(&m, func_index, f);
                                    otter_vm_exec::compile_one_pending_request(
                                        crate::jit_runtime::runtime_helpers(),
                                    );
                                }
                                crate::jit_runtime::JitCallResult::BailoutRestart
                                | crate::jit_runtime::JitCallResult::NotCompiled => {}
                            }
                        }

                        if has_rest {
                            let mut args = ctx.take_pending_args();
                            let rest_args: Vec<Value> = if args.len() > param_count {
                                args.drain(param_count..).collect()
                            } else {
                                Vec::new()
                            };
                            let rest_arr = GcRef::new(JsObject::array(rest_args.len()));
                            if let Some(array_obj) =
                                ctx.get_global("Array").and_then(|v| v.as_object())
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
                            module_id,
                            local_count,
                            Some(return_reg),
                            is_construct,
                            is_async,
                            argc as u16,
                        )?;
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
                            let f = m.function(func_index).ok_or_else(|| {
                                VmError::internal(format!(
                                    "callee not found (func_index={}, function_count={})",
                                    func_index,
                                    m.function_count()
                                ))
                            })?;
                            (f.local_count, f.flags.has_rest, f.param_count as usize)
                        };

                        if has_rest {
                            let mut args = ctx.take_pending_args();
                            let rest_args: Vec<Value> = if args.len() > param_count {
                                args.drain(param_count..).collect()
                            } else {
                                Vec::new()
                            };
                            let rest_arr = GcRef::new(JsObject::array(rest_args.len()));
                            if let Some(array_obj) =
                                ctx.get_global("Array").and_then(|v| v.as_object())
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
                            module_id,
                            local_count,
                            Some(return_reg),
                            false,
                            is_async,
                            argc as u16,
                        )?;
                    }
                    DispatchAction::Suspend {
                        promise,
                        resume_reg,
                    } => {
                        ctx.advance_pc();
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
                                return Ok(Value::promise(promise));
                            }
                        }
                    }
                    DispatchAction::Yield { value, .. } => {
                        let result = GcRef::new(JsObject::new(Value::null()));
                        let _ = result.set(PropertyKey::string("value"), value);
                        let _ = result.set(PropertyKey::string("done"), Value::boolean(false));
                        ctx.advance_pc();
                        return Ok(Value::object(result));
                    }
                }
            } else {
                // Continue: advance PC
                if let Some(f) = ctx.call_stack.last_mut() {
                    f.pc += 1;
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
    ) -> VmResult<()> {
        match instruction {
            // ==================== Constants ====================
            Instruction::LoadUndefined { dst } => {
                ctx.set_register(dst.0, Value::undefined());
                Ok(())
            }

            Instruction::LoadNull { dst } => {
                ctx.set_register(dst.0, Value::null());
                Ok(())
            }

            Instruction::LoadTrue { dst } => {
                ctx.set_register(dst.0, Value::boolean(true));
                Ok(())
            }

            Instruction::LoadFalse { dst } => {
                ctx.set_register(dst.0, Value::boolean(false));
                Ok(())
            }

            Instruction::LoadInt8 { dst, value } => {
                ctx.set_register(dst.0, Value::int32(*value as i32));
                Ok(())
            }

            Instruction::LoadInt32 { dst, value } => {
                ctx.set_register(dst.0, Value::int32(*value));
                Ok(())
            }

            Instruction::LoadConst { dst, idx } => {
                let constant = module
                    .constants
                    .get(idx.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;

                // Fast path: non-global/non-sticky RegExp literals are cached by (module_ptr, const_idx).
                // Global/sticky RegExps have mutable lastIndex so we must not share a single instance.
                let value = if let otter_vm_bytecode::Constant::RegExp { flags, .. } = constant {
                    let is_stateful = flags.contains('g') || flags.contains('y');
                    if is_stateful {
                        self.constant_to_value(ctx, constant)?
                    } else {
                        let module_id = module.module_id;
                        if let Some(cached) = ctx.get_cached_regexp(module_id, idx.0) {
                            cached
                        } else {
                            let val = self.constant_to_value(ctx, constant)?;
                            ctx.cache_regexp(module_id, idx.0, val);
                            val
                        }
                    }
                } else {
                    self.constant_to_value(ctx, constant)?
                };

                ctx.set_register(dst.0, value);
                Ok(())
            }

            // ==================== Variables ====================
            Instruction::GetLocal { dst, idx } => {
                ctx.load_local_into_register(dst.0, idx.0);
                Ok(())
            }

            Instruction::SetLocal { idx, src } => {
                ctx.store_register_into_local(idx.0, src.0);
                Ok(())
            }

            Instruction::GetLocal2 {
                dst1,
                idx1,
                dst2,
                idx2,
            } => {
                ctx.load_local_into_register(dst1.0, idx1.0);
                ctx.load_local_into_register(dst2.0, idx2.0);
                Ok(())
            }

            Instruction::IncLocal { local_idx, src } => {
                // Fused Inc + SetLocal: increment src register and store to local
                if let Some(value) = ctx.get_register(src.0).as_int32()
                    && let Some(result) = value.checked_add(1)
                {
                    let v = Value::int32(result);
                    ctx.set_register(src.0, v);
                    ctx.store_register_into_local(local_idx.0, src.0);
                    return Ok(());
                }

                let value = *ctx.get_register(src.0);
                if let Some(num) = value.as_number() {
                    let v = Value::number(num + 1.0);
                    ctx.set_register(src.0, v);
                    ctx.store_register_into_local(local_idx.0, src.0);
                    return Ok(());
                }

                let numeric = self.to_numeric(ctx, &value)?;
                match numeric {
                    Numeric::BigInt(bigint) => {
                        let result = bigint + NumBigInt::one();
                        let v = Value::bigint(result.to_string());
                        ctx.set_register(src.0, v);
                    }
                    Numeric::Number(num) => {
                        let v = Value::number(num + 1.0);
                        ctx.set_register(src.0, v);
                    }
                }
                ctx.store_register_into_local(local_idx.0, src.0);
                Ok(())
            }

            Instruction::GetUpvalue { dst, idx } => {
                // Get value from upvalue cell
                let value = ctx.get_upvalue(idx.0)?;
                ctx.set_register(dst.0, value);
                Ok(())
            }

            Instruction::SetUpvalue { idx, src } => {
                // Set value in upvalue cell
                let value = *ctx.get_register(src.0);
                ctx.set_upvalue(idx.0, value)?;
                Ok(())
            }

            Instruction::CloseUpvalue { local_idx } => {
                // Close the upvalue: sync local value to cell and remove from open set
                ctx.close_upvalue(local_idx.0)?;
                Ok(())
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
                    if global_obj.is_dictionary_mode() {
                        None
                    } else {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let feedback = frame.feedback().read();
                        if let Some(ic) = feedback.get(*ic_index as usize) {
                            if let otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                shape_id: shape_addr,
                                offset,
                                ..
                            } = &ic.ic_state
                            {
                                if global_obj.shape_id() == *shape_addr {
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
                    ctx.set_register(dst.0, value);
                    return Ok(());
                }

                let value = match ctx.get_global_utf16(name_str) {
                    Some(value) => value,
                    None => {
                        let message =
                            format!("{} is not defined", String::from_utf16_lossy(name_str));
                        let error = self.make_error(ctx, "ReferenceError", &message);
                        ctx.dispatch_action = Some(DispatchAction::Throw(error));
                        return Ok(());
                    }
                };

                // Update IC
                {
                    let global_obj = ctx.global();
                    if !global_obj.is_dictionary_mode() {
                        let key = Self::utf16_key(name_str);
                        if let Some(offset) = global_obj.shape_get_offset(&key) {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            let feedback = frame.feedback().write();
                            if let Some(ic) = feedback.get_mut(*ic_index as usize)
                                && matches!(
                                    ic.ic_state,
                                    otter_vm_bytecode::function::InlineCacheState::Uninitialized
                                )
                            {
                                ic.ic_state =
                                    otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                        shape_id: std::sync::Arc::as_ptr(&global_obj.shape())
                                            as u64,
                                        proto_shape_id: 0,
                                        depth: 0,
                                        offset: offset as u32,
                                    };
                            }
                        }
                    }
                }

                ctx.set_register(dst.0, value);
                Ok(())
            }

            Instruction::DeclareGlobalVar { name, configurable } => {
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;
                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;
                let key = Self::utf16_key(name_str);
                // Track var-declared names for GlobalDeclarationInstantiation collision checks
                ctx.add_global_var_name(String::from_utf16_lossy(name_str));
                let global = ctx.global();
                if !global.has_own(&key) {
                    // Per spec: B.3.3.2 (global script) uses CreateGlobalFunctionBinding(F, undefined, false)
                    // → configurable=false. B.3.3.3 (eval) uses configurable=true.
                    use crate::object::{PropertyAttributes, PropertyDescriptor};
                    global.define_property(
                        key,
                        PropertyDescriptor::data_with_attrs(
                            Value::undefined(),
                            PropertyAttributes {
                                writable: true,
                                enumerable: true,
                                configurable: *configurable,
                            },
                        ),
                    );
                }
                Ok(())
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
                let val_val = *ctx.get_register(src.0);

                // IC Fast Path
                {
                    let global_obj = ctx.global();
                    if !global_obj.is_dictionary_mode() {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let feedback = frame.feedback().read();
                        if let Some(ic) = feedback.get(*ic_index as usize)
                            && let otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                shape_id: shape_addr,
                                offset,
                                ..
                            } = &ic.ic_state
                            && global_obj.shape_id() == *shape_addr
                            && global_obj.set_by_offset(*offset as usize, val_val).is_ok()
                        {
                            return Ok(());
                        }
                    }
                }

                // Strict mode: ReferenceError on assignment to undeclared variable
                if !is_declaration {
                    let is_strict = ctx
                        .current_frame()
                        .and_then(|frame| module.function(frame.function_index))
                        .map(|func| func.flags.is_strict)
                        .unwrap_or(false);

                    if is_strict {
                        let global_obj = ctx.global();
                        let key = Self::utf16_key(name_str);
                        let property_exists = if global_obj.is_dictionary_mode() {
                            global_obj.has_own(&key)
                        } else {
                            global_obj.shape_get_offset(&key).is_some()
                        };
                        if !property_exists {
                            return Err(VmError::ReferenceError(format!(
                                "{} is not defined",
                                String::from_utf16_lossy(name_str)
                            )));
                        }
                    }
                }

                ctx.set_global_utf16(name_str, val_val);

                // Update IC
                {
                    let global_obj = ctx.global();
                    if !global_obj.is_dictionary_mode() {
                        let key = Self::utf16_key(name_str);
                        if let Some(offset) = global_obj.shape_get_offset(&key) {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            let feedback = frame.feedback().write();
                            if let Some(ic) = feedback.get_mut(*ic_index as usize)
                                && matches!(
                                    ic.ic_state,
                                    otter_vm_bytecode::function::InlineCacheState::Uninitialized
                                )
                            {
                                ic.ic_state =
                                    otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                        shape_id: std::sync::Arc::as_ptr(&global_obj.shape())
                                            as u64,
                                        proto_shape_id: 0,
                                        depth: 0,
                                        offset: offset as u32,
                                    };
                            }
                        }
                    }
                }

                Ok(())
            }

            Instruction::LoadThis { dst } => {
                // In derived constructors, `this` is not available until super() is called
                if let Some(frame) = ctx.current_frame()
                    && frame.flags.is_derived()
                    && !frame.flags.this_initialized()
                {
                    return Err(VmError::ReferenceError(
                            "Must call super constructor in derived class before accessing 'this' or returning from derived constructor".to_string(),
                        ));
                }
                let this_value = ctx.this_value();
                ctx.set_register(dst.0, this_value);
                Ok(())
            }

            Instruction::ToNumber { dst, src } => {
                let value = *ctx.get_register(src.0);
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
                Ok(())
            }
            Instruction::ToString { dst, src } => {
                let value_ref = ctx.get_register(src.0);

                if let Some(s) = value_ref.as_string() {
                    ctx.set_register(dst.0, Value::string(s));
                    return Ok(());
                }

                if let Some(int_val) = value_ref.as_int32()
                    && (0..=9).contains(&int_val)
                {
                    let cached_str = match int_val {
                        0 => JsString::intern("0"),
                        1 => JsString::intern("1"),
                        2 => JsString::intern("2"),
                        3 => JsString::intern("3"),
                        4 => JsString::intern("4"),
                        5 => JsString::intern("5"),
                        6 => JsString::intern("6"),
                        7 => JsString::intern("7"),
                        8 => JsString::intern("8"),
                        9 => JsString::intern("9"),
                        _ => unreachable!(),
                    };
                    ctx.set_register(dst.0, Value::string(cached_str));
                    return Ok(());
                }

                let value = *value_ref;
                let s = self.to_string_value(ctx, &value)?;
                ctx.set_register(dst.0, Value::string(JsString::intern(&s)));
                Ok(())
            }

            Instruction::RequireCoercible { src } => {
                let value = *ctx.get_register(src.0);
                if value.is_null() {
                    return Err(VmError::type_error("Cannot destructure 'null' value"));
                }
                if value.is_undefined() {
                    return Err(VmError::type_error("Cannot destructure 'undefined' value"));
                }
                Ok(())
            }

            Instruction::ThrowIfNotObject { src } => {
                let value = *ctx.get_register(src.0);
                if !value.is_object() && !value.is_proxy() && value.as_function().is_none() {
                    return Err(VmError::type_error(
                        "Iterator result is not an object",
                    ));
                }
                Ok(())
            }

            // ==================== Arithmetic ====================
            Instruction::Add {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if let Some(ty) = Self::get_arithmetic_fast_path(ctx, *feedback_index) {
                    use otter_vm_bytecode::function::ArithmeticType;
                    match ty {
                        ArithmeticType::Int32 => {
                            let fast_result = {
                                let left = ctx.get_register(lhs.0);
                                let right = ctx.get_register(rhs.0);
                                (left.as_int32(), right.as_int32())
                            };
                            if let (Some(l), Some(r)) = fast_result
                                && let Some(result) = l.checked_add(r)
                            {
                                ctx.set_register(dst.0, Value::int32(result));
                                return Ok(());
                            }
                        }
                        ArithmeticType::Number => {
                            let fast_result = {
                                let left = ctx.get_register(lhs.0);
                                let right = ctx.get_register(rhs.0);
                                (left.as_number(), right.as_number())
                            };
                            if let (Some(l), Some(r)) = fast_result {
                                ctx.set_register(dst.0, Value::number(l + r));
                                return Ok(());
                            }
                        }
                        ArithmeticType::String => {
                            let uses_string = {
                                let left = ctx.get_register(lhs.0);
                                let right = ctx.get_register(rhs.0);
                                left.is_string() || right.is_string()
                            };
                            if uses_string {
                                let left = *ctx.get_register(lhs.0);
                                let right = *ctx.get_register(rhs.0);
                                let result = self.op_add(ctx, &left, &right)?;
                                ctx.set_register(dst.0, result);
                                return Ok(());
                            }
                        }
                    }
                }

                // Generic path
                let left = *ctx.get_register(lhs.0);
                let right = *ctx.get_register(rhs.0);
                let result = self.op_add(ctx, &left, &right)?;
                ctx.set_register(dst.0, result);
                Self::update_arithmetic_ic(ctx, *feedback_index, &left, &right);
                Ok(())
            }

            Instruction::Sub {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if let Some(ty) = Self::get_arithmetic_fast_path(ctx, *feedback_index) {
                    use otter_vm_bytecode::function::ArithmeticType;
                    match ty {
                        ArithmeticType::Int32 => {
                            let fast_result = {
                                let left = ctx.get_register(lhs.0);
                                let right = ctx.get_register(rhs.0);
                                (left.as_int32(), right.as_int32())
                            };
                            if let (Some(l), Some(r)) = fast_result
                                && let Some(result) = l.checked_sub(r)
                            {
                                ctx.set_register(dst.0, Value::int32(result));
                                return Ok(());
                            }
                        }
                        ArithmeticType::Number => {
                            let fast_result = {
                                let left = ctx.get_register(lhs.0);
                                let right = ctx.get_register(rhs.0);
                                (left.as_number(), right.as_number())
                            };
                            if let (Some(l), Some(r)) = fast_result {
                                ctx.set_register(dst.0, Value::number(l - r));
                                return Ok(());
                            }
                        }
                        _ => {}
                    }
                }

                // Generic path (ToNumeric)
                let left_value = *ctx.get_register(lhs.0);
                let right_value = *ctx.get_register(rhs.0);
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
                Self::update_arithmetic_ic(ctx, *feedback_index, &left_value, &right_value);
                Ok(())
            }

            Instruction::Inc { dst, src } => {
                if let Some(value) = ctx.get_register(src.0).as_int32()
                    && let Some(result) = value.checked_add(1)
                {
                    ctx.set_register(dst.0, Value::int32(result));
                    return Ok(());
                }

                let value = *ctx.get_register(src.0);
                if let Some(num) = value.as_number() {
                    ctx.set_register(dst.0, Value::number(num + 1.0));
                    return Ok(());
                }

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
                Ok(())
            }

            Instruction::Dec { dst, src } => {
                if let Some(value) = ctx.get_register(src.0).as_int32()
                    && let Some(result) = value.checked_sub(1)
                {
                    ctx.set_register(dst.0, Value::int32(result));
                    return Ok(());
                }

                let value = *ctx.get_register(src.0);
                if let Some(num) = value.as_number() {
                    ctx.set_register(dst.0, Value::number(num - 1.0));
                    return Ok(());
                }

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
                Ok(())
            }

            Instruction::Mul {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                // Check-first: if IC already knows this is int32, skip feedback write
                if let Some(ty) = Self::get_arithmetic_fast_path(ctx, *feedback_index) {
                    use otter_vm_bytecode::function::ArithmeticType;
                    match ty {
                        ArithmeticType::Int32 => {
                            let fast_result = {
                                let left = ctx.get_register(lhs.0);
                                let right = ctx.get_register(rhs.0);
                                (left.as_int32(), right.as_int32())
                            };
                            if let (Some(l), Some(r)) = fast_result
                                && let Some(result) = l.checked_mul(r)
                            {
                                ctx.set_register(dst.0, Value::int32(result));
                                return Ok(());
                            }
                        }
                        ArithmeticType::Number => {
                            let fast_result = {
                                let left = ctx.get_register(lhs.0);
                                let right = ctx.get_register(rhs.0);
                                (left.as_number(), right.as_number())
                            };
                            if let (Some(l), Some(r)) = fast_result {
                                ctx.set_register(dst.0, Value::number(l * r));
                                return Ok(());
                            }
                        }
                        _ => {}
                    }
                }

                // Slow path: observe types and update feedback
                Self::update_arithmetic_ic(
                    ctx,
                    *feedback_index,
                    &ctx.get_register(lhs.0).clone(),
                    &ctx.get_register(rhs.0).clone(),
                );

                // Generic path (ToNumeric)
                let left_value = *ctx.get_register(lhs.0);
                let right_value = *ctx.get_register(rhs.0);
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
                Ok(())
            }

            Instruction::Div {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                // Check-first: if IC already knows this is int32/number, skip feedback write
                if let Some(ty) = Self::get_arithmetic_fast_path(ctx, *feedback_index) {
                    use otter_vm_bytecode::function::ArithmeticType;
                    match ty {
                        ArithmeticType::Int32 => {
                            let fast_result = {
                                let left = ctx.get_register(lhs.0);
                                let right = ctx.get_register(rhs.0);
                                (left.as_int32(), right.as_int32())
                            };
                            if let (Some(l), Some(r)) = fast_result
                                && let Some(rem) = l.checked_rem(r)
                                && rem == 0
                                && let Some(quotient) = l.checked_div(r)
                            {
                                ctx.set_register(dst.0, Value::int32(quotient));
                                return Ok(());
                            }
                        }
                        ArithmeticType::Number => {
                            let fast_result = {
                                let left = ctx.get_register(lhs.0);
                                let right = ctx.get_register(rhs.0);
                                (left.as_number(), right.as_number())
                            };
                            if let (Some(l), Some(r)) = fast_result {
                                ctx.set_register(dst.0, Value::number(l / r));
                                return Ok(());
                            }
                        }
                        _ => {}
                    }
                }

                // Slow path: observe types and update feedback
                Self::update_arithmetic_ic(
                    ctx,
                    *feedback_index,
                    &ctx.get_register(lhs.0).clone(),
                    &ctx.get_register(rhs.0).clone(),
                );

                // Generic path (ToNumeric)
                let left_value = *ctx.get_register(lhs.0);
                let right_value = *ctx.get_register(rhs.0);
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
                Ok(())
            }

            Instruction::Mod { dst, lhs, rhs } => {
                let left_value = *ctx.get_register(lhs.0);
                let right_value = *ctx.get_register(rhs.0);
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
                Ok(())
            }

            Instruction::Pow { dst, lhs, rhs } => {
                let left_value = *ctx.get_register(lhs.0);
                let right_value = *ctx.get_register(rhs.0);
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
                Ok(())
            }

            Instruction::Neg { dst, src } => {
                let val = *ctx.get_register(src.0);
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
                Ok(())
            }

            // ==================== Comparison ====================
            Instruction::Eq { dst, lhs, rhs } => {
                let left = *ctx.get_register(lhs.0);
                let right = *ctx.get_register(rhs.0);

                let result = self.abstract_equal(ctx, &left, &right)?;
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(())
            }

            Instruction::Ne { dst, lhs, rhs } => {
                let left = *ctx.get_register(lhs.0);
                let right = *ctx.get_register(rhs.0);

                let result = !self.abstract_equal(ctx, &left, &right)?;
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(())
            }

            Instruction::StrictEq { dst, lhs, rhs } => {
                let result = {
                    let left = ctx.get_register(lhs.0);
                    let right = ctx.get_register(rhs.0);
                    self.strict_equal(left, right)
                };
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(())
            }

            Instruction::StrictNe { dst, lhs, rhs } => {
                let result = {
                    let left = ctx.get_register(lhs.0);
                    let right = ctx.get_register(rhs.0);
                    !self.strict_equal(left, right)
                };
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(())
            }

            Instruction::Lt { dst, lhs, rhs } => {
                let fast_result = {
                    let left = ctx.get_register(lhs.0);
                    let right = ctx.get_register(rhs.0);
                    if let (Some(left), Some(right)) = (left.as_int32(), right.as_int32()) {
                        Some(left < right)
                    } else if let (Some(left), Some(right)) = (left.as_number(), right.as_number())
                    {
                        Some(left < right)
                    } else {
                        None
                    }
                };
                if let Some(result) = fast_result {
                    ctx.set_register(dst.0, Value::boolean(result));
                    return Ok(());
                }

                let left_val = *ctx.get_register(lhs.0);
                let right_val = *ctx.get_register(rhs.0);
                let left = self.to_numeric(ctx, &left_val)?;
                let right = self.to_numeric(ctx, &right_val)?;
                let result = matches!(self.numeric_compare(left, right)?, Some(Ordering::Less));

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(())
            }

            Instruction::Le { dst, lhs, rhs } => {
                let fast_result = {
                    let left = ctx.get_register(lhs.0);
                    let right = ctx.get_register(rhs.0);
                    if let (Some(left), Some(right)) = (left.as_int32(), right.as_int32()) {
                        Some(left <= right)
                    } else if let (Some(left), Some(right)) = (left.as_number(), right.as_number())
                    {
                        Some(left <= right)
                    } else {
                        None
                    }
                };
                if let Some(result) = fast_result {
                    ctx.set_register(dst.0, Value::boolean(result));
                    return Ok(());
                }

                let left_val = *ctx.get_register(lhs.0);
                let right_val = *ctx.get_register(rhs.0);
                let left = self.to_numeric(ctx, &left_val)?;
                let right = self.to_numeric(ctx, &right_val)?;
                let result = matches!(
                    self.numeric_compare(left, right)?,
                    Some(Ordering::Less | Ordering::Equal)
                );

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(())
            }

            Instruction::Gt { dst, lhs, rhs } => {
                let fast_result = {
                    let left = ctx.get_register(lhs.0);
                    let right = ctx.get_register(rhs.0);
                    if let (Some(left), Some(right)) = (left.as_int32(), right.as_int32()) {
                        Some(left > right)
                    } else if let (Some(left), Some(right)) = (left.as_number(), right.as_number())
                    {
                        Some(left > right)
                    } else {
                        None
                    }
                };
                if let Some(result) = fast_result {
                    ctx.set_register(dst.0, Value::boolean(result));
                    return Ok(());
                }

                let left_val = *ctx.get_register(lhs.0);
                let right_val = *ctx.get_register(rhs.0);
                let left = self.to_numeric(ctx, &left_val)?;
                let right = self.to_numeric(ctx, &right_val)?;
                let result = matches!(self.numeric_compare(left, right)?, Some(Ordering::Greater));

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(())
            }

            Instruction::Ge { dst, lhs, rhs } => {
                let fast_result = {
                    let left = ctx.get_register(lhs.0);
                    let right = ctx.get_register(rhs.0);
                    if let (Some(left), Some(right)) = (left.as_int32(), right.as_int32()) {
                        Some(left >= right)
                    } else if let (Some(left), Some(right)) = (left.as_number(), right.as_number())
                    {
                        Some(left >= right)
                    } else {
                        None
                    }
                };
                if let Some(result) = fast_result {
                    ctx.set_register(dst.0, Value::boolean(result));
                    return Ok(());
                }

                let left_val = *ctx.get_register(lhs.0);
                let right_val = *ctx.get_register(rhs.0);
                let left = self.to_numeric(ctx, &left_val)?;
                let right = self.to_numeric(ctx, &right_val)?;
                let result = matches!(
                    self.numeric_compare(left, right)?,
                    Some(Ordering::Greater | Ordering::Equal)
                );

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(())
            }

            // ==================== Logical ====================
            Instruction::Not { dst, src } => {
                let value = ctx.get_register(src.0).to_boolean();
                ctx.set_register(dst.0, Value::boolean(!value));
                Ok(())
            }

            // ==================== Type Operations ====================
            Instruction::TypeOf { dst, src } => {
                let type_name = ctx.get_register(src.0).type_of();
                let str_value = Value::string(JsString::intern(type_name));
                ctx.set_register(dst.0, str_value);
                Ok(())
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
                let str_value = Value::string(JsString::intern(type_name));
                ctx.set_register(dst.0, str_value);
                Ok(())
            }

            Instruction::InstanceOf {
                dst,
                lhs,
                rhs,
                ic_index,
            } => {
                let left = *ctx.get_register(lhs.0);
                let right = *ctx.get_register(rhs.0);

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
                    let result = self.call_function(ctx, &handler, right, &[left])?;
                    // Step 2c: Return ToBoolean(result)
                    ctx.set_register(dst.0, Value::boolean(result.to_boolean()));
                    return Ok(());
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
                    return Ok(());
                };

                // IC Fast Path - cache the prototype property lookup on the constructor
                let proto_key = PropertyKey::string("prototype");
                let mut cached_proto = None;
                {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let feedback = frame.feedback().write();
                    if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                        use otter_vm_bytecode::function::InlineCacheState;
                        let obj_shape_ptr = right_obj.shape_id();

                        if ic.proto_epoch_matches(ctx.cached_proto_epoch) {
                            match &mut ic.ic_state {
                                InlineCacheState::Monomorphic {
                                    shape_id, offset, ..
                                } => {
                                    if obj_shape_ptr == *shape_id {
                                        cached_proto = right_obj.get_by_offset(*offset as usize);
                                    }
                                }
                                InlineCacheState::Polymorphic { count, entries } => {
                                    for i in 0..(*count as usize) {
                                        if obj_shape_ptr == entries[i].0 {
                                            cached_proto =
                                                right_obj.get_by_offset(entries[i].3 as usize);
                                            // MRU: promote to front
                                            if i > 0 {
                                                entries.swap(0, i);
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

                let proto_val = if let Some(val) = cached_proto {
                    val
                } else {
                    // Slow path: full lookup and IC update
                    let proto = right_obj.get(&proto_key).unwrap_or_else(Value::undefined);

                    // Update IC
                    if let Some(offset) = right_obj.shape_get_offset(&proto_key) {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let feedback = frame.feedback().write();
                        if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                            use otter_vm_bytecode::function::InlineCacheState;
                            // Skip IC for dictionary mode objects
                            if right_obj.is_dictionary_mode() {
                                ic.ic_state = InlineCacheState::Megamorphic;
                            } else {
                                let shape_ptr = right_obj.shape_id();
                                let current_epoch = ctx.cached_proto_epoch;

                                match &mut ic.ic_state {
                                    InlineCacheState::Uninitialized => {
                                        ic.ic_state = InlineCacheState::Monomorphic {
                                            shape_id: shape_ptr,
                                            proto_shape_id: 0,
                                            depth: 0,
                                            offset: offset as u32,
                                        };
                                        ic.proto_epoch = current_epoch;
                                    }
                                    InlineCacheState::Monomorphic {
                                        shape_id: old_shape,
                                        offset: old_offset,
                                        ..
                                    } => {
                                        if *old_shape != shape_ptr {
                                            let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                                            entries[0] = (*old_shape, 0, 0, *old_offset);
                                            entries[1] = (shape_ptr, 0, 0, offset as u32);
                                            ic.ic_state =
                                                InlineCacheState::Polymorphic { count: 2, entries };
                                            ic.proto_epoch = current_epoch;
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        let mut found = false;
                                        for entry in &entries[..(*count as usize)] {
                                            if entry.0 == shape_ptr {
                                                found = true;
                                                break;
                                            }
                                        }
                                        if !found {
                                            if (*count as usize) < 4 {
                                                entries[*count as usize] =
                                                    (shape_ptr, 0, 0, offset as u32);
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
                        return Ok(());
                    }
                    depth += 1;
                    if depth > MAX_PROTO_DEPTH {
                        break;
                    }
                    current = obj.prototype().as_object();
                }

                ctx.set_register(dst.0, Value::boolean(false));
                Ok(())
            }

            Instruction::In {
                dst,
                lhs,
                rhs,
                ic_index: _ic_index,
            } => {
                let left = *ctx.get_register(lhs.0);
                let right = *ctx.get_register(rhs.0);

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
                        crate::proxy_operations::proxy_has(&mut ncx, proxy, &key, left)?
                    };
                    ctx.set_register(dst.0, Value::boolean(result));
                    return Ok(());
                }

                // TypedArray [[HasProperty]] — check before as_object()
                if let Some(ta) = right.as_typed_array() {
                    let key = self.value_to_property_key(ctx, &left)?;
                    match typed_array_ops::ta_has(&ta, &key) {
                        TaHasResult::Present => {
                            ctx.set_register(dst.0, Value::boolean(true));
                            return Ok(());
                        }
                        TaHasResult::Absent => {
                            ctx.set_register(dst.0, Value::boolean(false));
                            return Ok(());
                        }
                        TaHasResult::NotAnIndex => {
                            // Fall through to ta.object for named property lookup
                            let result = self.has_with_proxy_chain(ctx, &ta.object, &key, left)?;
                            ctx.set_register(dst.0, Value::boolean(result));
                            return Ok(());
                        }
                    }
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

                let result = self.has_with_proxy_chain(ctx, &right_obj, &key, left)?;
                ctx.set_register(dst.0, Value::boolean(result));
                Ok(())
            }

            // ==================== Control Flow ====================
            Instruction::Jump { offset } => {
                ctx.dispatch_action = Some(DispatchAction::Jump(offset.0));
                Ok(())
            }

            Instruction::JumpIfTrue { cond, offset } => {
                if ctx.get_register(cond.0).to_boolean() {
                    {
                        ctx.dispatch_action = Some(DispatchAction::Jump(offset.0));
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }

            Instruction::JumpIfFalse { cond, offset } => {
                if !ctx.get_register(cond.0).to_boolean() {
                    {
                        ctx.dispatch_action = Some(DispatchAction::Jump(offset.0));
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }

            Instruction::JumpIfNullish { src, offset } => {
                if ctx.get_register(src.0).is_nullish() {
                    {
                        ctx.dispatch_action = Some(DispatchAction::Jump(offset.0));
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }

            Instruction::JumpIfNotNullish { src, offset } => {
                if !ctx.get_register(src.0).is_nullish() {
                    {
                        ctx.dispatch_action = Some(DispatchAction::Jump(offset.0));
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }

            Instruction::JumpTable { index_reg, targets } => {
                let val = *ctx.get_register(index_reg.0);
                if let Some(n) = val.as_number() {
                    let idx = n as usize;
                    if idx < targets.len() {
                        if let Some(offset) = targets.get(idx) {
                            ctx.dispatch_action = Some(DispatchAction::Jump(offset.0));
                        }
                    }
                }
                Ok(())
            }

            // ==================== Exception Handling ====================
            Instruction::TryStart { catch_offset } => {
                let pc = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame"))?
                    .pc;
                let catch_pc = (pc as i32 + catch_offset.0) as usize;
                ctx.push_try(catch_pc);
                Ok(())
            }

            Instruction::TryEnd => {
                ctx.pop_try_for_current_frame();
                Ok(())
            }

            Instruction::Throw { src } => {
                let value = *ctx.get_register(src.0);
                ctx.dispatch_action = Some(DispatchAction::Throw(value));
                Ok(())
            }

            Instruction::Catch { dst } => {
                let value = ctx.take_exception().unwrap_or_else(Value::undefined);
                ctx.set_register(dst.0, value);
                Ok(())
            }

            // ==================== Functions ====================
            Instruction::Closure { dst, func } => {
                // Get the function definition to know what upvalues to capture
                let func_def = module
                    .function(func.0)
                    .ok_or_else(|| VmError::internal("function not found for closure"))?;

                // Capture upvalues from parent frame
                let captured_upvalues = self.capture_upvalues(ctx, &func_def.upvalues)?;

                let func_obj = GcRef::new(JsObject::new(Value::null()));

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
                    let _ = proto.set(PropertyKey::string("constructor"), func_value);
                }
                ctx.set_register(dst.0, func_value);
                Ok(())
            }

            Instruction::AsyncClosure { dst, func } => {
                let _mm = ctx.memory_manager().clone();
                // Get the function definition to know what upvalues to capture
                let func_def = module
                    .function(func.0)
                    .ok_or_else(|| VmError::internal("function not found for async closure"))?;

                // Capture upvalues from parent frame
                let captured_upvalues = self.capture_upvalues(ctx, &func_def.upvalues)?;

                let func_obj = GcRef::new(JsObject::new(Value::null()));

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
                Ok(())
            }

            Instruction::GeneratorClosure { dst, func } => {
                let _mm = ctx.memory_manager().clone();
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

                // Create the .prototype for instances — inherits from %GeneratorPrototype%
                let gen_proto = ctx
                    .realm_intrinsics(ctx.realm_id())
                    .map(|i| Value::object(i.generator_prototype))
                    .unwrap_or_else(Value::null);
                let proto = GcRef::new(JsObject::new(gen_proto));

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
                let _ = proto.set(PropertyKey::string("constructor"), func_value);
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
                Ok(())
            }

            Instruction::AsyncGeneratorClosure { dst, func } => {
                let _mm = ctx.memory_manager().clone();
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

                // Create the .prototype for instances — inherits from %AsyncGeneratorPrototype%
                let async_gen_proto = ctx
                    .realm_intrinsics(ctx.realm_id())
                    .map(|i| Value::object(i.async_generator_prototype))
                    .unwrap_or_else(Value::null);
                let proto = GcRef::new(JsObject::new(async_gen_proto));

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
                let _ = proto.set(PropertyKey::string("constructor"), func_value);
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
                Ok(())
            }

            Instruction::Call {
                dst,
                func,
                argc,
                ic_index,
            } => {
                let func_value = *ctx.get_register(func.0);

                // Fast path: direct closure call (non-generator) avoids generic call dispatch.
                if let Some(closure) = func_value.as_function()
                    && !closure.is_generator
                {
                    // Record call target IC (callee_bits + func_index + module_id + is_async).
                    // Uses callee_bits to skip recording on monomorphic hit (same closure).
                    // JIT uses this data for monomorphic call specialization.
                    if *ic_index > 0
                        && let Some(frame) = ctx.current_frame()
                    {
                        let feedback = frame.feedback().write();
                        if let Some(md) = feedback.get_mut(*ic_index as usize) {
                            let bits = func_value.to_bits_raw();
                            if md.callee_bits != bits {
                                // First call or callee changed — update IC
                                let func_idx = closure.function_index;
                                let mod_id = closure.module.module_id;
                                if md.call_target_func_index == 0 {
                                    md.callee_bits = bits;
                                    md.call_target_func_index = func_idx.wrapping_add(1);
                                    md.call_target_module_id = mod_id;
                                    md.call_target_is_async = closure.is_async;
                                } else {
                                    md.call_target_func_index = u32::MAX; // megamorphic
                                }
                            }
                            // else: callee_bits match → monomorphic, skip recording
                        }
                    }

                    ctx.set_pending_args_from_register_range(func.0 + 1, *argc as u16);
                    let realm_id = self.realm_id_for_function(ctx, &func_value);
                    ctx.set_pending_realm_id(realm_id);
                    ctx.set_pending_this(Value::undefined());
                    if let Some(ref home_object) = closure.home_object {
                        ctx.set_pending_home_object(*home_object);
                    }
                    ctx.set_pending_callee_value(func_value);

                    ctx.dispatch_action = Some(DispatchAction::Call {
                        func_index: closure.function_index,
                        module_id: closure.module.module_id,
                        argc: *argc,
                        return_reg: dst.0,
                        is_construct: false,
                        is_async: closure.is_async,
                        upvalues: closure.upvalues.clone(),
                    });
                    return Ok(());
                }

                // Collect arguments upfront (used by multiple paths)
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    let arg = *ctx.get_register(func.0 + 1 + i);
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
                    return Ok(());
                }

                // Check if it's a native function first
                if let Some(native_fn) = func_value.as_native_function() {
                    // Some native ops need interpreter-level dispatch (call/apply, generator ops).
                    let is_same_native = |candidate: &Value| -> bool {
                        match (func_value.as_native_fn_obj(), candidate.as_native_fn_obj()) {
                            (Some(a), Some(b)) => std::ptr::eq(a.as_ptr(), b.as_ptr()),
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

                    // Record FFI call target IC for JIT fast path
                    if *ic_index > 0
                        && let Some(ffi_info_ptr) = func_value.ffi_call_info()
                        && let Some(frame) = ctx.current_frame()
                    {
                        let feedback = frame.feedback().write();
                        if let Some(md) = feedback.get_mut(*ic_index as usize) {
                            let bits = func_value.to_bits_raw();
                            if md.callee_bits != bits {
                                if md.ffi_call_info_ptr == 0 {
                                    md.callee_bits = bits;
                                    md.ffi_call_info_ptr = ffi_info_ptr as u64;
                                } else {
                                    // Multiple FFI targets → megamorphic
                                    md.ffi_call_info_ptr = 0;
                                    md.call_target_func_index = u32::MAX;
                                }
                            }
                        }
                    }

                    // Call the native function with depth tracking
                    let result = self.call_native_fn(ctx, native_fn, &Value::undefined(), &args)?;
                    ctx.set_register(dst.0, result);
                    return Ok(());
                }

                // Check if it's a bound function (object with __boundFunction__)
                if let Some(obj) = func_value.as_object()
                    && let Some(bound_fn) = obj.get(&PropertyKey::string("__boundFunction__"))
                {
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
                        && let Some(args_obj) = bound_args_val.as_object()
                    {
                        let len =
                            if let Some(len_val) = args_obj.get(&PropertyKey::string("length")) {
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

                    // Add call-time arguments
                    for i in 0..(*argc as u16) {
                        all_args.push(*ctx.get_register(func.0 + 1 + i));
                    }

                    // Call the bound function with the bound this and combined args
                    if let Some(native_fn) = bound_fn.as_native_function() {
                        // For native functions, we can't set 'this' directly
                        // but most native functions don't use 'this'
                        let result = self.call_native_fn(ctx, native_fn, &this_arg, &all_args)?;
                        ctx.set_register(dst.0, result);
                        return Ok(());
                    } else if let Some(closure) = bound_fn.as_function() {
                        // Set the bound this and args
                        let argc = all_args.len() as u8;
                        ctx.set_pending_this(this_arg);
                        ctx.set_pending_args_from_vec(all_args);

                        ctx.dispatch_action = Some(DispatchAction::Call {
                            func_index: closure.function_index,
                            module_id: closure.module.module_id,
                            argc,
                            return_reg: dst.0,
                            is_construct: false,
                            is_async: closure.is_async,
                            upvalues: closure.upvalues.clone(),
                        });
                        return Ok(());
                    } else {
                        return Err(VmError::type_error("bound function target is not callable"));
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
                let func_value = *ctx.get_register(func.0);
                let this_value = *ctx.get_register(this.0);

                // Fast path: direct closure call (non-generator) with explicit receiver.
                if let Some(closure) = func_value.as_function()
                    && !closure.is_generator
                {
                    ctx.set_pending_args_from_register_range(func.0 + 1, *argc as u16);
                    let realm_id = self.realm_id_for_function(ctx, &func_value);
                    ctx.set_pending_realm_id(realm_id);
                    ctx.set_pending_this(this_value);
                    if let Some(ref home_object) = closure.home_object {
                        ctx.set_pending_home_object(*home_object);
                    }
                    ctx.set_pending_callee_value(func_value);

                    ctx.dispatch_action = Some(DispatchAction::Call {
                        func_index: closure.function_index,
                        module_id: closure.module.module_id,
                        argc: *argc,
                        return_reg: dst.0,
                        is_construct: false,
                        is_async: closure.is_async,
                        upvalues: closure.upvalues.clone(),
                    });
                    return Ok(());
                }

                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    let arg = *ctx.get_register(func.0 + 1 + i);
                    args.push(arg);
                }

                self.handle_call_value(ctx, &func_value, this_value, args, dst.0)
            }

            Instruction::TailCall { func, argc } => {
                let func_value = *ctx.get_register(func.0);

                // Native functions don't benefit from tail call optimization
                // (they execute immediately), so just call and return
                if let Some(native_fn) = func_value.as_native_function() {
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        let arg = *ctx.get_register(func.0 + 1 + i);
                        args.push(arg);
                    }
                    let result = self.call_native_fn(ctx, native_fn, &Value::undefined(), &args)?;
                    ctx.dispatch_action = Some(DispatchAction::Return(result));
                    return Ok(());
                }

                // For closures, return TailCall result to reuse the frame
                if let Some(closure) = func_value.as_function() {
                    ctx.set_pending_args_from_register_range(func.0 + 1, *argc as u16);
                    ctx.set_pending_this(Value::undefined());

                    // Get the return register from the current frame (where tail call result goes)
                    let return_reg = ctx
                        .current_frame()
                        .and_then(|f| f.return_register)
                        .unwrap_or(0);

                    ctx.dispatch_action = Some(DispatchAction::TailCall {
                        func_index: closure.function_index,
                        module_id: closure.module.module_id,
                        argc: *argc,
                        return_reg,
                        is_async: closure.is_async,
                        upvalues: closure.upvalues.clone(),
                    });
                    return Ok(());
                }

                Err(VmError::type_error("not a function"))
            }

            Instruction::Construct { dst, func, argc } => {
                let func_value = *ctx.get_register(func.0);

                // Check if it's a proxy with construct trap
                if let Some(proxy) = func_value.as_proxy() {
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        let arg = *ctx.get_register(func.0 + 1 + i);
                        args.push(arg);
                    }
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_construct(
                            &mut ncx, proxy, &args, func_value, // new.target
                        )?
                    };
                    ctx.set_register(dst.0, result);
                    return Ok(());
                }

                if let Some(func_obj) = func_value.as_object()
                    && let Some(crate::object::PropertyDescriptor::Data { value, .. }) = func_obj
                        .get_own_property_descriptor(&PropertyKey::string("__non_constructor"))
                    && value.as_boolean() == Some(true)
                {
                    return Err(VmError::type_error("not a constructor"));
                }

                if let Some(native_fn) = func_value.as_native_function() {
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        let arg = *ctx.get_register(func.0 + 1 + i);
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
                        ctor_proto.map(Value::object).unwrap_or_else(Value::null),
                    ));
                    let new_obj_value = Value::object(new_obj);

                    // Capture stack trace for Error objects
                    if let Some(proto) = ctor_proto
                        && proto
                            .get(&PropertyKey::string("__is_error__"))
                            .and_then(|v| v.as_boolean())
                            == Some(true)
                    {
                        Self::capture_error_stack_trace(new_obj, ctx);
                    }

                    // Call native constructor with depth tracking
                    let result =
                        self.call_native_fn_construct(ctx, native_fn, &new_obj_value, &args)?;
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
                    return Ok(());
                }

                // Check if it's a callable constructor
                if let Some(closure) = func_value.as_function() {
                    // Check if this is a derived constructor (class extends)
                    let func_def = closure
                        .module
                        .functions
                        .get(closure.function_index as usize);
                    let is_derived = func_def.map(|f| f.flags.is_derived).unwrap_or(false);

                    if is_derived {
                        // Derived constructor: `this` is NOT created here.
                        // It will be created by super() call inside the constructor.
                        // Set pending_is_derived so the CallFrame knows.
                        ctx.set_pending_args_from_register_range(func.0 + 1, *argc as u16);
                        ctx.set_pending_this(Value::undefined());
                        ctx.set_pending_is_derived(true);

                        // Set callee_value so CallSuper can find the super constructor
                        // via Object.getPrototypeOf(callee) (static inheritance chain)
                        ctx.set_pending_callee_value(func_value);

                        // Set home_object = the constructor's .prototype
                        // (used by super() to find the parent constructor)
                        if let Some(ctor_obj) = func_value.as_object() {
                            let proto_key = PropertyKey::string("prototype");
                            if let Some(proto_val) = ctor_obj.get(&proto_key)
                                && let Some(proto_obj) = proto_val.as_object()
                            {
                                ctx.set_pending_home_object(proto_obj);
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
                            ctor_proto.map(Value::object).unwrap_or_else(Value::null),
                        ));
                        let new_obj_value = Value::object(new_obj);

                        // Capture stack trace for Error objects
                        if let Some(proto) = ctor_proto
                            && proto
                                .get(&PropertyKey::string("__is_error__"))
                                .and_then(|v| v.as_boolean())
                                == Some(true)
                        {
                            Self::capture_error_stack_trace(new_obj, ctx);
                        }

                        ctx.set_pending_args_from_register_range(func.0 + 1, *argc as u16);
                        ctx.set_pending_this(new_obj_value);

                        // Pre-set dst to the new object (will be returned if constructor returns undefined)
                        ctx.set_register(dst.0, new_obj_value);
                    }

                    let realm_id = self.realm_id_for_function(ctx, &func_value);
                    ctx.set_pending_realm_id(realm_id);
                    ctx.dispatch_action = Some(DispatchAction::Call {
                        func_index: closure.function_index,
                        module_id: closure.module.module_id,
                        argc: *argc,
                        return_reg: dst.0,
                        is_construct: true,
                        is_async: closure.is_async,
                        upvalues: closure.upvalues.clone(),
                    });
                    Ok(())
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
                let receiver = *ctx.get_register(obj.0);
                let method_const = module
                    .constants
                    .get(method.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;
                let method_name = method_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;

                // IC Fast Path — must handle depth > 0 for prototype properties
                let cached_method = if let Some(obj_ref) = receiver.as_object() {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let feedback = frame.feedback().read();
                    if let Some(ic) = feedback.get(*ic_index as usize) {
                        if ic.proto_epoch_matches(ctx.cached_proto_epoch) {
                            match &ic.ic_state {
                                otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                    shape_id: shape_addr,
                                    depth,
                                    offset,
                                    ..
                                } => {
                                    if obj_ref.shape_id() == *shape_addr {
                                        if *depth == 0 {
                                            obj_ref.get_by_offset(*offset as usize)
                                        } else {
                                            get_proto_value_at_depth(&obj_ref, *depth, *offset)
                                        }
                                    } else {
                                        None
                                    }
                                }
                                otter_vm_bytecode::function::InlineCacheState::Polymorphic {
                                    count,
                                    entries,
                                } => {
                                    let shape = obj_ref.shape_id();
                                    let mut result = None;
                                    for i in 0..(*count as usize) {
                                        if shape == entries[i].0 {
                                            let depth = entries[i].2;
                                            let offset = entries[i].3;
                                            result = if depth == 0 {
                                                obj_ref.get_by_offset(offset as usize)
                                            } else {
                                                get_proto_value_at_depth(&obj_ref, depth, offset)
                                            };
                                            break;
                                        }
                                    }
                                    result
                                }
                                _ => None,
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
                    if self.try_fast_path_array_method(
                        ctx,
                        &method_value,
                        &receiver,
                        *argc as u16,
                        obj.0 + 1,
                        dst.0,
                    )? {
                        return Ok(());
                    }

                    // Direct closure dispatch: bypass handle_call_value entirely
                    // when we know the method is a non-generator closure.
                    // Saves: Vec alloc for args, bound-function unwrap, native check.
                    if let Some(closure) = method_value.as_function()
                        && !closure.is_generator
                    {
                        ctx.set_pending_args_from_register_range(obj.0 + 1, *argc as u16);
                        let realm_id = self.realm_id_for_function(ctx, &method_value);
                        ctx.set_pending_realm_id(realm_id);
                        ctx.set_pending_this(receiver);
                        if let Some(ref home_object) = closure.home_object {
                            ctx.set_pending_home_object(*home_object);
                        }
                        ctx.set_pending_callee_value(method_value);

                        // Record call target IC for this method call site
                        if let Some(frame) = ctx.current_frame()
                            && let Some(md) = frame.feedback().write().get_mut(*ic_index as usize)
                        {
                            let func_idx = closure.function_index;
                            let mod_id = closure.module.module_id;
                            if md.call_target_func_index == 0 {
                                md.callee_bits = method_value.to_bits_raw();
                                md.call_target_func_index = func_idx.wrapping_add(1);
                                md.call_target_module_id = mod_id;
                                md.call_target_is_async = closure.is_async;
                            } else if md.call_target_func_index != func_idx.wrapping_add(1)
                                || md.call_target_module_id != mod_id
                            {
                                md.call_target_func_index = u32::MAX;
                            }
                        }

                        ctx.dispatch_action = Some(DispatchAction::Call {
                            func_index: closure.function_index,
                            module_id: closure.module.module_id,
                            argc: *argc,
                            return_reg: dst.0,
                            is_construct: false,
                            is_async: closure.is_async,
                            upvalues: closure.upvalues.clone(),
                        });
                        return Ok(());
                    }

                    // Fallback: native functions, bound functions, generators
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        args.push(*ctx.get_register(obj.0 + 1 + i));
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
                    crate::proxy_operations::proxy_get(&mut ncx, proxy, &key, key_value, receiver)?
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
                    let key = Self::utf16_key(method_name);

                    if !obj_ref.is_dictionary_mode()
                        && let Some(offset) = obj_ref.shape_get_offset(&key)
                    {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let feedback = frame.feedback().write();
                        if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                            use otter_vm_bytecode::function::InlineCacheState;
                            let shape_ptr = obj_ref.shape_id();
                            let current_epoch = ctx.cached_proto_epoch;

                            match &mut ic.ic_state {
                                InlineCacheState::Uninitialized => {
                                    ic.ic_state = InlineCacheState::Monomorphic {
                                        shape_id: shape_ptr,
                                        proto_shape_id: 0,
                                        depth: 0,
                                        offset: offset as u32,
                                    };
                                    ic.proto_epoch = current_epoch;
                                }
                                InlineCacheState::Monomorphic {
                                    shape_id: old_shape,
                                    offset: old_offset,
                                    ..
                                } => {
                                    if *old_shape != shape_ptr {
                                        let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                                        entries[0] = (*old_shape, 0, 0, *old_offset);
                                        entries[1] = (shape_ptr, 0, 0, offset as u32);
                                        ic.ic_state =
                                            InlineCacheState::Polymorphic { count: 2, entries };
                                        ic.proto_epoch = current_epoch;
                                    }
                                }
                                InlineCacheState::Polymorphic { count, entries } => {
                                    let mut found = false;
                                    for entry in &entries[..(*count as usize)] {
                                        if entry.0 == shape_ptr {
                                            found = true;
                                            break;
                                        }
                                    }
                                    if !found {
                                        if (*count as usize) < 4 {
                                            entries[*count as usize] =
                                                (shape_ptr, 0, 0, offset as u32);
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

                    obj_ref.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_string() {
                    let proto = ctx
                        .string_prototype()
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
                            Some(*ctx.get_register(obj.0 + 1))
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
                                    generator.set_pending_throw(error_value);
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
                            return Ok(());
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
                        let result = GcRef::new(JsObject::new(Value::null()));
                        let _ = result.set(PropertyKey::string("value"), result_value);
                        let _ = result.set(PropertyKey::string("done"), Value::boolean(is_done));
                        ctx.set_register(dst.0, Value::object(result));
                        return Ok(());
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
                        crate::proxy_operations::proxy_get(&mut ncx, proxy, &key, key_val, receiver)
                    };
                    result.unwrap_or_default()
                } else {
                    return Err(VmError::type_error("Cannot read property of non-object"));
                };

                if self.try_fast_path_array_method(
                    ctx,
                    &method_value,
                    &receiver,
                    *argc as u16,
                    obj.0 + 1,
                    dst.0,
                )? {
                    return Ok(());
                }

                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    args.push(*ctx.get_register(obj.0 + 1 + i));
                }

                // Update IC if method was found on the object itself
                if let Some(obj_ref) = receiver.as_object() {
                    let key = Self::utf16_key(method_name);
                    if let Some(offset) = obj_ref.shape_get_offset(&key) {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let feedback = frame.feedback().write();
                        if let Some(ic) = feedback.get_mut(*ic_index as usize)
                            && matches!(
                                ic.ic_state,
                                otter_vm_bytecode::function::InlineCacheState::Uninitialized
                            )
                        {
                            ic.ic_state =
                                otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                                    shape_id: obj_ref.shape_id(),
                                    proto_shape_id: 0,
                                    depth: 0,
                                    offset: offset as u32,
                                };
                        }
                    }
                }

                self.handle_call_value(ctx, &method_value, receiver, args, dst.0)
            }

            Instruction::Return { src } => {
                let value = *ctx.get_register(src.0);
                // In derived constructors:
                // - returning an object is OK
                // - returning undefined after super() was called: return this
                // - returning non-object or undefined without super(): error
                if let Some(frame) = ctx.current_frame()
                    && frame.flags.is_derived()
                {
                    if value.is_object() {
                        // Explicit object return is fine
                    } else if value.is_undefined() && frame.flags.this_initialized() {
                        // Implicit/explicit undefined return → return this
                        ctx.dispatch_action = Some(DispatchAction::Return(frame.this_value));
                        return Ok(());
                    } else if !frame.flags.this_initialized() {
                        return Err(VmError::ReferenceError(
                                "Must call super constructor in derived class before returning from derived constructor".to_string(),
                            ));
                    }
                    // Non-object, non-undefined explicit return in derived: TypeError per spec
                    // but for now treat as returning undefined → this
                }
                ctx.dispatch_action = Some(DispatchAction::Return(value));
                Ok(())
            }

            Instruction::ReturnUndefined => {
                // In derived constructors, implicit return should return `this`
                if let Some(frame) = ctx.current_frame()
                    && frame.flags.is_derived()
                {
                    if !frame.flags.this_initialized() {
                        return Err(VmError::ReferenceError(
                                "Must call super constructor in derived class before returning from derived constructor".to_string(),
                            ));
                    }
                    // Return this_value (the object created by super())
                    ctx.dispatch_action = Some(DispatchAction::Return(frame.this_value));
                    return Ok(());
                }
                ctx.dispatch_action = Some(DispatchAction::Return(Value::undefined()));
                Ok(())
            }

            Instruction::CallSpread {
                dst,
                func,
                argc,
                spread,
            } => {
                let func_value = *ctx.get_register(func.0);
                let spread_arr = *ctx.get_register(spread.0);

                // Collect regular arguments first
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    let arg = *ctx.get_register(func.0 + 1 + i);
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
                    return Ok(());
                }

                // Regular closure call
                let closure = func_value
                    .as_function()
                    .ok_or_else(|| VmError::type_error("not a function"))?;

                // Store args in context for new frame to pick up
                let argc = args.len() as u8;
                ctx.set_pending_args_from_vec(args);

                ctx.dispatch_action = Some(DispatchAction::Call {
                    func_index: closure.function_index,
                    module_id: closure.module.module_id,
                    argc,
                    return_reg: dst.0,
                    is_construct: false,
                    is_async: closure.is_async,
                    upvalues: closure.upvalues.clone(),
                });
                Ok(())
            }

            Instruction::ConstructSpread {
                dst,
                func,
                argc,
                spread,
            } => {
                let func_value = *ctx.get_register(func.0);
                let spread_arr = *ctx.get_register(spread.0);

                // Collect regular arguments first
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    let arg = *ctx.get_register(func.0 + 1 + i);
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
                    return Ok(());
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
                ));
                let new_obj_value = Value::object(new_obj);

                let argc_u8 = args.len() as u8;
                ctx.set_pending_args_from_vec(args);
                ctx.set_pending_this(new_obj_value);
                ctx.set_register(dst.0, new_obj_value);

                ctx.dispatch_action = Some(DispatchAction::Call {
                    func_index: closure.function_index,
                    module_id: closure.module.module_id,
                    argc: argc_u8,
                    return_reg: dst.0,
                    is_construct: true,
                    is_async: closure.is_async,
                    upvalues: closure.upvalues.clone(),
                });
                Ok(())
            }

            // ==================== Async/Await ====================
            Instruction::Await { dst, src } => {
                let value = *ctx.get_register(src.0);

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
                            Ok(())
                        }
                        PromiseState::Rejected(error) => {
                            // Promise rejected — throw the rejection value as-is
                            // (ES2023 §27.7.5.3 Await step 5: if rejected, throw reason)
                            Err(VmError::exception(error))
                        }
                        PromiseState::Pending | PromiseState::PendingThenable(_) => {
                            // Promise is pending, suspend execution
                            ctx.dispatch_action = Some(DispatchAction::Suspend {
                                promise,
                                resume_reg: dst.0,
                            });
                            Ok(())
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
                            let promise_ref = promise;
                            let promise_ref2 = promise;
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
                                    Ok(())
                                }
                                PromiseState::Rejected(error) => Err(VmError::exception(error)),
                                PromiseState::Pending | PromiseState::PendingThenable(_) => {
                                    ctx.dispatch_action = Some(DispatchAction::Suspend {
                                        promise,
                                        resume_reg: dst.0,
                                    });
                                    Ok(())
                                }
                            }
                        } else {
                            // Has .then but it's not callable — not a thenable
                            ctx.set_register(dst.0, value);
                            Ok(())
                        }
                    } else {
                        // No .then property — not a thenable
                        ctx.set_register(dst.0, value);
                        Ok(())
                    }
                } else {
                    // Primitive non-Promise — return directly
                    ctx.set_register(dst.0, value);
                    Ok(())
                }
            }

            Instruction::Yield { dst, src } => {
                let value = *ctx.get_register(src.0);

                // Yield suspends the generator and returns the value
                // The dst register will receive the value sent to next() on resumption
                // (handled in resume_generatorution using yield_dst)

                // Return a yield result with the destination register
                ctx.dispatch_action = Some(DispatchAction::Yield {
                    value,
                    yield_dst: dst.0,
                });
                Ok(())
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

                // Use shared root shape (V8/JSC pattern): all objects created
                // by NewObject share the same initial shape, making ICs
                // monomorphic for uniform construction like `{ a: 1, b: 2 }`.
                let obj = GcRef::new(JsObject::new_with_shared_shape(
                    proto.map(Value::object).unwrap_or_else(Value::null),
                ));
                ctx.set_register(dst.0, Value::object(obj));
                Ok(())
            }

            Instruction::CreateArguments { dst } => {
                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame"))?;
                let argc = frame.argc as usize;
                let register_base = frame.register_base;
                let extra_args_offset = frame.extra_args_offset as usize;
                let extra_args_count = frame.extra_args_count as usize;
                let func = &module.functions[frame.function_index as usize];
                let param_count = func.param_count as usize;
                let local_count = func.local_count as usize;
                let is_strict = func.flags.is_strict;
                let is_mapped = !is_strict && func.flags.has_simple_parameters;
                let callee_val = frame.callee_value;
                let mm = ctx.memory_manager().clone();

                // Get Object.prototype for the arguments object
                let obj_proto = ctx
                    .get_global("Object")
                    .and_then(|v| v.as_object())
                    .and_then(|o| o.get(&PropertyKey::string("prototype")))
                    .and_then(|v| v.as_object());

                // Use array_like (not array) so Array.isArray(arguments) returns false
                let args_obj = GcRef::new(JsObject::array_like(argc));
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
                            let extra_index = i - param_count;
                            let val = if extra_index < extra_args_count {
                                ctx.get_absolute_slot(
                                    register_base + extra_args_offset + extra_index,
                                )?
                            } else {
                                let offset = local_count + extra_index;
                                ctx.get_local(offset as u16)?
                        };
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
                            let extra_index = i - param_count;
                            if extra_index < extra_args_count {
                                ctx.get_absolute_slot(register_base + extra_args_offset + extra_index)?
                            } else {
                                let offset = local_count + extra_index;
                                ctx.get_local(offset as u16)?
                            }
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
                                get: Some(thrower),
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
                    && let Some(iterator_fn) = array_proto.get(&PropertyKey::Symbol(iterator_sym))
                {
                    args_obj.define_property(
                        PropertyKey::Symbol(iterator_sym),
                        PropertyDescriptor::data_with_attrs(
                            iterator_fn,
                            PropertyAttributes::builtin_method(),
                        ),
                    );
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
                Ok(())
            }

            Instruction::CallEval { dst, code } => {
                let code_value = *ctx.get_register(code.0);

                // Per spec §19.2.1.1: if argument is not a string, return it unchanged
                if !code_value.is_string() {
                    ctx.set_register(dst.0, code_value);
                    return Ok(());
                }

                let js_str = code_value
                    .as_string()
                    .ok_or_else(|| VmError::type_error("eval argument is not a string"))?;
                let source = js_str.as_str().to_string();

                // Per ES2023 §19.2.1.1: Direct eval inherits strict mode from calling context
                let is_strict_context = ctx
                    .current_frame()
                    .and_then(|frame| module.functions.get(frame.function_index as usize))
                    .map(|func| func.flags.is_strict)
                    .unwrap_or(false);

                // Direct eval inherits the caller's this binding.
                let caller_this = ctx.this_value();
                ctx.set_pending_this(caller_this);

                let injected_eval_bindings = self.inject_eval_bindings(ctx);

                // Detect if eval is running inside a function (not at global/module level).
                // Function-scope eval needs cleanup of any new global properties afterwards.
                let is_function_scope_eval = ctx.stack_depth() > 1;
                let global_keys_before = if is_function_scope_eval {
                    Some(ctx.global().own_keys())
                } else {
                    None
                };

                let eval_result = (|| {
                    let timer1 = std::time::Instant::now();
                    let eval_module = ctx.compile_eval(&source, is_strict_context)?;
                    if timer1.elapsed().as_millis() > 50 {
                        println!(
                            "SLOW compile_eval for {:?} took {:?}",
                            source,
                            timer1.elapsed()
                        );
                    }
                    let timer2 = std::time::Instant::now();
                    let result = self.execute_eval_module(ctx, &eval_module);
                    if timer2.elapsed().as_millis() > 50 {
                        println!("SLOW execute_eval_module took {:?}", timer2.elapsed());
                    }
                    result
                })();
                self.cleanup_eval_bindings(
                    ctx,
                    &injected_eval_bindings,
                    global_keys_before.as_deref(),
                );

                let result = eval_result?;
                ctx.set_register(dst.0, result);
                Ok(())
            }

            Instruction::Import {
                dst,
                module: module_idx,
            } => {
                let value = ctx.host_import_from_constant_pool(&module.constants, module_idx.0)?;
                ctx.set_register(dst.0, value);
                Ok(())
            }

            Instruction::Export { name, src } => {
                let value = *ctx.get_register(src.0);
                ctx.host_export_from_constant_pool(&module.constants, name.0, value)?;
                Ok(())
            }

            Instruction::ForInNext { dst, obj, offset } => {
                let target = *ctx.get_register(obj.0);
                match ctx.host_for_in_next(target)? {
                    Some(value) => {
                        ctx.set_register(dst.0, value);
                        Ok(())
                    }
                    None => {
                        ctx.dispatch_action = Some(DispatchAction::Jump(offset.0));
                        Ok(())
                    }
                }
            }

            Instruction::GetPropConst {
                dst,
                obj,
                name,
                ic_index,
            } => {
                let object = *ctx.get_register(obj.0);

                // Hot path: IC probe on regular object
                if let Some(obj_ref) = object.as_object()
                    && !obj_ref.is_dictionary_mode()
                {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let feedback = frame.feedback().write();
                    if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                        use otter_vm_bytecode::function::InlineCacheState;
                        let obj_shape_ptr = obj_ref.shape_id();

                        if ic.proto_epoch_matches(ctx.cached_proto_epoch) {
                            match &mut ic.ic_state {
                                InlineCacheState::Monomorphic {
                                    shape_id,
                                    depth,
                                    offset,
                                    ..
                                } => {
                                    if obj_shape_ptr == *shape_id {
                                        let val = if *depth == 0 {
                                            obj_ref.get_by_offset(*offset as usize)
                                        } else {
                                            get_proto_value_at_depth(&obj_ref, *depth, *offset)
                                        };
                                        if let Some(val) = val {
                                            ctx.set_register(dst.0, val);
                                            return Ok(());
                                        }
                                    }
                                }
                                InlineCacheState::Polymorphic { count, entries } => {
                                    for i in 0..(*count as usize) {
                                        if obj_shape_ptr == entries[i].0 {
                                            let depth = entries[i].2;
                                            let offset = entries[i].3;
                                            let val = if depth == 0 {
                                                obj_ref.get_by_offset(offset as usize)
                                            } else {
                                                get_proto_value_at_depth(&obj_ref, depth, offset)
                                            };
                                            if i > 0 && val.is_some() {
                                                entries.swap(0, i);
                                            }
                                            if let Some(val) = val {
                                                ctx.set_register(dst.0, val);
                                                return Ok(());
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

                // Cold slow path: proxy, string, array.length, IC miss, autoboxing
                self.getprop_const_slow(ctx, module, *dst, *obj, *name, *ic_index, object)
            }

            Instruction::SetPropConst {
                obj,
                name,
                val,
                ic_index,
            } => {
                let obj_reg = *obj;
                let object = *ctx.get_register(obj.0);
                let is_strict = ctx
                    .current_frame()
                    .and_then(|frame| module.function(frame.function_index))
                    .map(|func| func.flags.is_strict)
                    .unwrap_or(false);
                let name_const = module
                    .constants
                    .get(name.0)
                    .ok_or_else(|| VmError::internal("constant not found"))?;
                let name_str = name_const
                    .as_string()
                    .ok_or_else(|| VmError::internal("expected string constant"))?;
                let val_val = *ctx.get_register(val.0);

                // Proxy check - must be first
                if let Some(proxy) = object.as_proxy() {
                    let key = Self::utf16_key(name_str);
                    let key_value = Value::string(JsString::intern_utf16(name_str));
                    let receiver = object;
                    {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_set(
                            &mut ncx, proxy, &key, key_value, val_val, receiver,
                        )?;
                    }
                    return Ok(());
                }

                if let Some(obj) = object.as_object() {
                    let key = Self::utf16_key(name_str);

                    // IC Fast Path
                    let mut cached = false;
                    {
                        let frame = ctx
                            .current_frame()
                            .ok_or_else(|| VmError::internal("no frame"))?;
                        let feedback = frame.feedback().write();
                        if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                            use otter_vm_bytecode::function::InlineCacheState;
                            let obj_shape_ptr = obj.shape_id();

                            if ic.proto_epoch_matches(ctx.cached_proto_epoch) {
                                match &mut ic.ic_state {
                                    InlineCacheState::Monomorphic {
                                        shape_id, offset, ..
                                    } => {
                                        if obj_shape_ptr == *shape_id {
                                            match obj.set_by_offset(*offset as usize, val_val) {
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
                                                match obj
                                                    .set_by_offset(entries[i].3 as usize, val_val)
                                                {
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
                                                // MRU: promote to front
                                                if i > 0 {
                                                    entries.swap(0, i);
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
                        return Ok(());
                    }

                    match obj.get_own_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Accessor { set, .. }) => {
                            let Some(setter) = set else {
                                if is_strict {
                                    return Err(VmError::type_error(
                                        "Cannot set property which has only a getter",
                                    ));
                                }
                                return Ok(());
                            };

                            if let Some(native_fn) = setter.as_native_function() {
                                self.call_native_fn(ctx, native_fn, &object, &[val_val])?;
                                Ok(())
                            } else if let Some(closure) = setter.as_function() {
                                ctx.set_pending_args_one(val_val);
                                ctx.set_pending_this(object);
                                ctx.dispatch_action = Some(DispatchAction::Call {
                                    func_index: closure.function_index,
                                    module_id: closure.module.module_id,
                                    argc: 1,
                                    return_reg: 0, // Setter return value is ignored
                                    is_construct: false,
                                    is_async: closure.is_async,
                                    upvalues: closure.upvalues.clone(),
                                });
                                Ok(())
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
                            Ok(())
                        }
                        _ => {
                            // Own data property: set directly
                            if let Err(e) = obj.set(key, val_val) {
                                if matches!(e, SetPropertyError::InvalidArrayLength) {
                                    return Err(VmError::range_error(e.to_string()));
                                }
                                if is_strict {
                                    return Err(VmError::type_error(e.to_string()));
                                }
                            }
                            // Skip IC for dictionary mode objects
                            if !obj.is_dictionary_mode()
                                && let Some(offset) =
                                    obj.shape_get_offset(&Self::utf16_key(name_str))
                            {
                                let frame = ctx
                                    .current_frame()
                                    .ok_or_else(|| VmError::internal("no frame"))?;
                                let feedback = frame.feedback().write();
                                if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                                    use otter_vm_bytecode::function::InlineCacheState;
                                    let shape_ptr = obj.shape_id();
                                    let current_epoch = ctx.cached_proto_epoch;

                                    match &mut ic.ic_state {
                                        InlineCacheState::Uninitialized => {
                                            ic.ic_state = InlineCacheState::Monomorphic {
                                                shape_id: shape_ptr,
                                                proto_shape_id: 0,
                                                depth: 0,
                                                offset: offset as u32,
                                            };
                                            ic.proto_epoch = current_epoch;
                                        }
                                        InlineCacheState::Monomorphic {
                                            shape_id: old_shape,
                                            offset: old_offset,
                                            ..
                                        } => {
                                            if *old_shape != shape_ptr {
                                                let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                                                entries[0] = (*old_shape, 0, 0, *old_offset);
                                                entries[1] = (shape_ptr, 0, 0, offset as u32);
                                                ic.ic_state = InlineCacheState::Polymorphic {
                                                    count: 2,
                                                    entries,
                                                };
                                                ic.proto_epoch = current_epoch;
                                            }
                                        }
                                        InlineCacheState::Polymorphic { count, entries } => {
                                            let mut found = false;
                                            for entry in &entries[..(*count as usize)] {
                                                if entry.0 == shape_ptr {
                                                    found = true;
                                                    break;
                                                }
                                            }
                                            if !found {
                                                if (*count as usize) < 4 {
                                                    entries[*count as usize] =
                                                        (shape_ptr, 0, 0, offset as u32);
                                                    *count += 1;
                                                    ic.proto_epoch = current_epoch;
                                                } else {
                                                    ic.ic_state = InlineCacheState::Megamorphic;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }

                                    // Quickening: when IC is monomorphic with enough hits,
                                    // quicken to SetPropQuickened
                                    ic.hit_count = ic.hit_count.saturating_add(1);
                                    if ic.hit_count
                                        >= otter_vm_bytecode::function::QUICKENING_WARMUP
                                        && let InlineCacheState::Monomorphic {
                                            shape_id,
                                            offset,
                                            ..
                                        } = ic.ic_state
                                        && let Some(func) = module.function(frame.function_index)
                                    {
                                        let pc = frame.pc;
                                        Self::try_quicken_property_access(
                                            func,
                                            pc,
                                            &Instruction::SetPropConst {
                                                obj: obj_reg,
                                                name: *name,
                                                val: *val,
                                                ic_index: *ic_index,
                                            },
                                            shape_id,
                                            offset,
                                            0, // SetProp always operates on the receiver
                                            ic.proto_epoch,
                                        );
                                    }
                                }
                            }
                            Ok(())
                        }
                    }
                } else {
                    if is_strict {
                        return Err(VmError::type_error("Cannot set property on non-object"));
                    }
                    Ok(())
                }
            }

            Instruction::DeleteProp { dst, obj, key } => {
                let object = *ctx.get_register(obj.0);
                let key_value = *ctx.get_register(key.0);

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
                    return Ok(());
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

                // TypedArray exotic [[Delete]] — §10.4.5.6
                if let Some(ta) = object.as_typed_array() {
                    let result = if let Some(del_result) = typed_array_ops::ta_delete(&ta, &prop_key) {
                        del_result
                    } else {
                        // Not a numeric index — ordinary delete on ta.object
                        ta.object.delete(&prop_key)
                    };
                    if !result {
                        let is_strict = ctx
                            .current_frame()
                            .and_then(|frame| module.function(frame.function_index))
                            .map(|func| func.flags.is_strict)
                            .unwrap_or(false);
                        if is_strict {
                            return Err(VmError::type_error(
                                "Cannot delete property of a TypedArray",
                            ));
                        }
                    }
                    ctx.set_register(dst.0, Value::boolean(result));
                    return Ok(());
                }

                let result = if let Some(obj) = object.as_object() {
                    if !obj.has_own(&prop_key) {
                        true
                    } else {
                        obj.delete(&prop_key)
                    }
                } else {
                    true
                };

                // Strict mode: throw TypeError if delete failed (non-configurable property)
                if !result {
                    let is_strict = ctx
                        .current_frame()
                        .and_then(|frame| module.function(frame.function_index))
                        .map(|func| func.flags.is_strict)
                        .unwrap_or(false);
                    if is_strict {
                        return Err(VmError::type_error(
                            "Cannot delete non-configurable property",
                        ));
                    }
                }

                ctx.set_register(dst.0, Value::boolean(result));
                Ok(())
            }

            Instruction::GetProp {
                dst,
                obj,
                key,
                ic_index: _ic_index,
            } => {
                let object = *ctx.get_register(obj.0);
                let key_value = *ctx.get_register(key.0);

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
                    let receiver = object;
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_get(
                            &mut ncx, proxy, &prop_key, key_value, receiver,
                        )?
                    };
                    ctx.set_register(dst.0, result);
                    return Ok(());
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
                            return Ok(());
                        }
                        PropertyKey::Index(index) => {
                            let units = str_ref.as_utf16();
                            if let Some(unit) = units.get(*index as usize) {
                                let ch = JsString::intern_utf16(&[*unit]);
                                ctx.set_register(dst.0, Value::string(ch));
                            } else {
                                ctx.set_register(dst.0, Value::undefined());
                            }
                            return Ok(());
                        }
                        _ => {}
                    }

                    if let Some(proto) = ctx.string_prototype() {
                        let value = proto.get(&key).unwrap_or_else(Value::undefined);
                        ctx.set_register(dst.0, value);
                        return Ok(());
                    }
                }

                // TypedArray [[Get]] — check before as_object()
                if let Some(ta) = object.as_typed_array() {
                    match typed_array_ops::value_to_canonical_index(&key_value) {
                        Some(typed_array_ops::CanonicalIndex::Int(idx)) => {
                            let val = ta.get_value(idx).unwrap_or(Value::undefined());
                            ctx.set_register(dst.0, val);
                            return Ok(());
                        }
                        Some(typed_array_ops::CanonicalIndex::NonInt) => {
                            // Canonical numeric but not valid index → undefined, no prototype lookup
                            ctx.set_register(dst.0, Value::undefined());
                            return Ok(());
                        }
                        None => {
                            // Not a canonical numeric index — fall through to OrdinaryGet on ta.object
                            let key = self.value_to_property_key(ctx, &key_value)?;
                            let key_val_for_proxy = crate::proxy_operations::property_key_to_value_pub(&key);
                            let value = self.get_with_proxy_chain(ctx, &ta.object, &key, key_val_for_proxy, &object)?;
                            ctx.set_register(dst.0, value);
                            return Ok(());
                        }
                    }
                }

                if let Some(obj) = object.as_object() {
                    let receiver = object;
                    // Convert key to property key
                    let key = self.value_to_property_key(ctx, &key_value)?;

                    match obj.lookup_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Accessor { get, .. }) => {
                            let Some(getter) = get else {
                                ctx.set_register(dst.0, Value::undefined());
                                return Ok(());
                            };

                            if let Some(native_fn) = getter.as_native_function() {
                                let result = self.call_native_fn(ctx, native_fn, &receiver, &[])?;
                                ctx.set_register(dst.0, result);
                                Ok(())
                            } else if let Some(closure) = getter.as_function() {
                                ctx.set_pending_args_empty();
                                ctx.set_pending_this(receiver);
                                ctx.dispatch_action = Some(DispatchAction::Call {
                                    func_index: closure.function_index,
                                    module_id: closure.module.module_id,
                                    argc: 0,
                                    return_reg: dst.0,
                                    is_construct: false,
                                    is_async: closure.is_async,
                                    upvalues: closure.upvalues.clone(),
                                });
                                Ok(())
                            } else {
                                Err(VmError::type_error("getter is not a function"))
                            }
                        }
                        _ => {
                            let value =
                                self.get_with_proxy_chain(ctx, &obj, &key, key_value, &receiver)?;
                            ctx.set_register(dst.0, value);
                            Ok(())
                        }
                    }
                } else if object.is_number() {
                    // Autobox number -> Number.prototype
                    let key = self.value_to_property_key(ctx, &key_value)?;
                    if let Some(number_obj) = ctx.get_global("Number").and_then(|v| v.as_object())
                        && let Some(proto) = number_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                    {
                        let value = proto.get(&key).unwrap_or_else(Value::undefined);
                        ctx.set_register(dst.0, value);
                        return Ok(());
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    Ok(())
                } else if object.is_boolean() {
                    // Autobox boolean -> Boolean.prototype
                    let key = self.value_to_property_key(ctx, &key_value)?;
                    if let Some(boolean_obj) = ctx.get_global("Boolean").and_then(|v| v.as_object())
                        && let Some(proto) = boolean_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                    {
                        let value = proto.get(&key).unwrap_or_else(Value::undefined);
                        ctx.set_register(dst.0, value);
                        return Ok(());
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    Ok(())
                } else if object.is_symbol() {
                    // Autobox symbol -> Symbol.prototype
                    let key = self.value_to_property_key(ctx, &key_value)?;
                    if let Some(symbol_obj) = ctx.get_global("Symbol").and_then(|v| v.as_object())
                        && let Some(proto) = symbol_obj
                            .get(&PropertyKey::string("prototype"))
                            .and_then(|v| v.as_object())
                    {
                        let value = self.get_property_value(ctx, &proto, &key, &object)?;
                        ctx.set_register(dst.0, value);
                        return Ok(());
                    }
                    ctx.set_register(dst.0, Value::undefined());
                    Ok(())
                } else {
                    ctx.set_register(dst.0, Value::undefined());
                    Ok(())
                }
            }

            Instruction::SetProp {
                obj,
                key,
                val,
                ic_index: _ic_index,
            } => {
                let object = *ctx.get_register(obj.0);
                let key_value = *ctx.get_register(key.0);
                let val_val = *ctx.get_register(val.0);
                let is_strict = ctx
                    .current_frame()
                    .and_then(|frame| module.function(frame.function_index))
                    .map(|func| func.flags.is_strict)
                    .unwrap_or(false);

                // Proxy check - must be first
                if let Some(proxy) = object.as_proxy() {
                    let prop_key = self.value_to_property_key(ctx, &key_value)?;
                    let receiver = object;
                    {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_set(
                            &mut ncx, proxy, &prop_key, key_value, val_val, receiver,
                        )?;
                    }
                    return Ok(());
                }

                // TypedArray [[Set]] — §10.4.5.5
                if let Some(ta) = object.as_typed_array() {
                    match typed_array_ops::value_to_canonical_index(&key_value) {
                        Some(typed_array_ops::CanonicalIndex::Int(idx)) => {
                            if ta.kind().is_bigint() {
                                let prim = if val_val.is_object() || val_val.as_object().is_some() {
                                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                                    ncx.to_primitive(&val_val, crate::interpreter::PreferredType::Number)?
                                } else {
                                    val_val
                                };
                                let n = crate::intrinsics_impl::typed_array::to_bigint_i64(&prim)?;
                                if !ta.is_detached() && idx < ta.length() {
                                    ta.set_bigint(idx, n);
                                }
                            } else {
                                let n = {
                                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                                    ncx.to_number_value(&val_val)?
                                };
                                if !ta.is_detached() && idx < ta.length() {
                                    ta.set(idx, n);
                                }
                            }
                            return Ok(());
                        }
                        Some(typed_array_ops::CanonicalIndex::NonInt) => {
                            // §10.4.5.11 IntegerIndexedElementSet: still call ToNumber/ToBigInt
                            // which can trigger side effects / throw
                            if ta.kind().is_bigint() {
                                let prim = if val_val.is_object() || val_val.as_object().is_some() {
                                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                                    ncx.to_primitive(&val_val, crate::interpreter::PreferredType::Number)?
                                } else {
                                    val_val
                                };
                                let _ = crate::intrinsics_impl::typed_array::to_bigint_i64(&prim)?;
                            } else {
                                let mut ncx = crate::context::NativeContext::new(ctx, self);
                                let _ = ncx.to_number_value(&val_val)?;
                            }
                            return Ok(());
                        }
                        None => {
                            let key = self.value_to_property_key(ctx, &key_value)?;
                            let _ = ta.object.set(key, val_val);
                            return Ok(());
                        }
                    }
                }

                if let Some(obj) = object.as_object() {
                    let key = self.value_to_property_key(ctx, &key_value)?;

                    // §10.4.5.5: TypedArray in prototype chain intercepts numeric index
                    // When TA's [[Set]] is called with O !== Receiver:
                    //   - If IsValidIntegerIndex(O, idx) is false → return true (do nothing)
                    //   - If IsValidIntegerIndex(O, idx) is true → OrdinarySet on receiver
                    if let Some(ci) = typed_array_ops::canonical_numeric_index(&key) {
                        let mut proto_val = obj.prototype();
                        let mut found_ta = false;
                        let mut ta_valid_index = false;
                        for _ in 0..64 {
                            if proto_val.is_null() || proto_val.is_undefined() { break; }
                            if let Some(ta) = proto_val.as_typed_array() {
                                found_ta = true;
                                match ci {
                                    typed_array_ops::CanonicalIndex::Int(idx) => {
                                        if !ta.is_detached() && idx < ta.length() {
                                            ta_valid_index = true;
                                        }
                                    }
                                    typed_array_ops::CanonicalIndex::NonInt => {
                                        // NonInt → IsValidIntegerIndex always false
                                    }
                                }
                                break;
                            }
                            if let Some(p) = proto_val.as_object() {
                                proto_val = p.prototype();
                            } else { break; }
                        }
                        if found_ta && !ta_valid_index {
                            // §10.4.5.5 step 2.b.ii: invalid index → return true (do nothing)
                            return Ok(());
                        }
                        if ta_valid_index {
                            // Valid index, receiver !== TA → OrdinarySet on receiver
                            let success = if obj.is_array() {
                                if let PropertyKey::Index(i) = &key {
                                    obj.set_index(*i as usize, val_val).is_ok()
                                } else {
                                    obj.set(key, val_val).is_ok()
                                }
                            } else if !obj.is_extensible() {
                                // Non-extensible: can't add new property
                                false
                            } else {
                                obj.define_property(
                                    key,
                                    PropertyDescriptor::data_with_attrs(
                                        val_val,
                                        PropertyAttributes {
                                            writable: true,
                                            enumerable: true,
                                            configurable: true,
                                        },
                                    ),
                                )
                            };
                            if !success && is_strict {
                                return Err(VmError::type_error(
                                    "Cannot add property to a non-extensible object",
                                ));
                            }
                            return Ok(());
                        }
                    }

                    match obj.lookup_property_descriptor(&key) {
                        Some(crate::object::PropertyDescriptor::Accessor { set, .. }) => {
                            let Some(setter) = set else {
                                return Ok(());
                            };

                            if let Some(native_fn) = setter.as_native_function() {
                                self.call_native_fn(ctx, native_fn, &object, &[val_val])?;
                                Ok(())
                            } else if let Some(closure) = setter.as_function() {
                                ctx.set_pending_args_one(val_val);
                                ctx.set_pending_this(object);
                                ctx.dispatch_action = Some(DispatchAction::Call {
                                    func_index: closure.function_index,
                                    module_id: closure.module.module_id,
                                    argc: 1,
                                    return_reg: 0, // Setter return value is ignored
                                    is_construct: false,
                                    is_async: closure.is_async,
                                    upvalues: closure.upvalues.clone(),
                                });
                                Ok(())
                            } else {
                                Err(VmError::type_error("setter is not a function"))
                            }
                        }
                        _ => {
                            // Slow path
                            if let Err(e) = obj.set(key, val_val) {
                                if matches!(e, SetPropertyError::InvalidArrayLength) {
                                    return Err(VmError::range_error(e.to_string()));
                                }
                                if is_strict {
                                    return Err(VmError::type_error(e.to_string()));
                                }
                            }
                            Ok(())
                        }
                    }
                } else {
                    if is_strict {
                        return Err(VmError::type_error("Cannot set property on non-object"));
                    }
                    Ok(())
                }
            }

            Instruction::DefineGetter { obj, key, func } => {
                let object = *ctx.get_register(obj.0);
                let key_value = *ctx.get_register(key.0);
                let getter_fn = *ctx.get_register(func.0);

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

                Ok(())
            }

            Instruction::DefineSetter { obj, key, func } => {
                let object = *ctx.get_register(obj.0);
                let key_value = *ctx.get_register(key.0);
                let setter_fn = *ctx.get_register(func.0);

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

                Ok(())
            }

            Instruction::DefineProperty { obj, key, val } => {
                let object = *ctx.get_register(obj.0);
                let key_value = *ctx.get_register(key.0);
                let value = *ctx.get_register(val.0);

                if let Some(obj) = object.as_object() {
                    let prop_key = self.value_to_property_key(ctx, &key_value)?;
                    obj.define_property(prop_key, PropertyDescriptor::data(value));
                }

                Ok(())
            }

            Instruction::DefineMethod { obj, key, val } => {
                let object = *ctx.get_register(obj.0);
                let key_value = *ctx.get_register(key.0);
                let value = *ctx.get_register(val.0);

                if let Some(obj) = object.as_object() {
                    let prop_key = self.value_to_property_key(ctx, &key_value)?;
                    obj.define_property(prop_key, PropertyDescriptor::builtin_method(value));
                }

                Ok(())
            }

            Instruction::SetPrototype { obj, proto } => {
                let object = *ctx.get_register(obj.0);
                let proto_value = *ctx.get_register(proto.0);

                if let Some(obj) = object.as_object()
                    && (proto_value.is_null() || proto_value.as_object().is_some())
                {
                    obj.set_prototype(proto_value);
                }

                Ok(())
            }

            // ==================== Arrays ====================
            Instruction::NewArray { dst, len, packed } => {
                let arr = GcRef::new(JsObject::array(*len as usize));
                arr.flags.borrow_mut().is_packed = *packed;
                // Attach `Array.prototype` if present so arrays are iterable and have methods.
                if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
                    && let Some(array_proto) = array_obj
                        .get(&PropertyKey::string("prototype"))
                        .and_then(|v| v.as_object())
                {
                    arr.set_prototype(Value::object(array_proto));
                }
                ctx.set_register(dst.0, Value::array(arr));
                Ok(())
            }

            Instruction::GetElemInt { dst, obj, index } => {
                let object = *ctx.get_register(obj.0);
                let idx_val = *ctx.get_register(index.0);

                // Proxy must be checked first — integer index access must go through proxy trap
                if let Some(proxy) = object.as_proxy() {
                    let prop_key = self.value_to_property_key(ctx, &idx_val)?;
                    let key_value = crate::proxy_operations::property_key_to_value_pub(&prop_key);
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_get(
                            &mut ncx, proxy, &prop_key, key_value, object,
                        )?
                    };
                    ctx.set_register(dst.0, result);
                    return Ok(());
                }

                // TypedArray fast path
                if let Some(ta) = object.as_typed_array()
                    && let Some(idx) = idx_val.as_int32()
                    && idx >= 0
                {
                    let val = ta.get_value(idx as usize).unwrap_or(Value::undefined());
                    ctx.set_register(dst.0, val);
                    return Ok(());
                }

                if let Some(idx) = idx_val.as_int32()
                    && idx >= 0
                    && let Some(obj_ref) = object.as_object()
                {
                    if let Some(val) = obj_ref.get_index(idx as usize) {
                        ctx.set_register(dst.0, val);
                        return Ok(());
                    }
                }

                // Fallback to generic GetElem semantics if fast path fails
                if let Some(obj_ref) = object.as_object() {
                    let prop_key = self.value_to_property_key(ctx, &idx_val)?;
                    let key_value = crate::proxy_operations::property_key_to_value_pub(&prop_key);
                    let result =
                        self.get_with_proxy_chain(ctx, &obj_ref, &prop_key, key_value, &object)?;
                    ctx.set_register(dst.0, result);
                } else {
                    ctx.set_register(dst.0, Value::undefined());
                }
                Ok(())
            }

            Instruction::GetElem {
                dst,
                arr,
                idx,
                ic_index,
            } => {
                let array = *ctx.get_register(arr.0);
                let index = *ctx.get_register(idx.0);

                // Proxy check - must be first
                if let Some(proxy) = array.as_proxy() {
                    let prop_key = self.value_to_property_key(ctx, &index)?;
                    let receiver = array;
                    let result = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_get(
                            &mut ncx, proxy, &prop_key, index, receiver,
                        )?
                    };
                    ctx.set_register(dst.0, result);
                    return Ok(());
                }

                // TypedArray [[Get]] — §10.4.5.4
                if let Some(ta) = array.as_typed_array() {
                    if let Some(idx) = typed_array_ops::value_to_numeric_index(&index) {
                        let val = ta.get_value(idx).unwrap_or(Value::undefined());
                        ctx.set_register(dst.0, val);
                        return Ok(());
                    }
                    // Non-numeric key: look up on ta.object (prototype chain)
                    let key = self.value_to_property_key(ctx, &index)?;
                    let key_val = crate::proxy_operations::property_key_to_value_pub(&key);
                    let value = self.get_with_proxy_chain(ctx, &ta.object, &key, key_val, &array)?;
                    ctx.set_register(dst.0, value);
                    return Ok(());
                }

                if let Some(obj) = array.as_object() {
                    // Fast path for numeric index access on arrays
                    if obj.is_array()
                        && let Some(n) = index.as_int32()
                        && n >= 0
                        && let Some(val) = obj.get_index(n as usize)
                    {
                        ctx.set_register(dst.0, val);
                        return Ok(());
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
                            let feedback = frame.feedback().write();
                            if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                                use otter_vm_bytecode::function::InlineCacheState;
                                let obj_shape_ptr = obj.shape_id();

                                if ic.proto_epoch_matches(ctx.cached_proto_epoch) {
                                    match &mut ic.ic_state {
                                        InlineCacheState::Monomorphic {
                                            shape_id, offset, ..
                                        } => {
                                            if obj_shape_ptr == *shape_id {
                                                cached_val = obj.get_by_offset(*offset as usize);
                                            }
                                        }
                                        InlineCacheState::Polymorphic { count, entries } => {
                                            for i in 0..(*count as usize) {
                                                if obj_shape_ptr == entries[i].0 {
                                                    cached_val =
                                                        obj.get_by_offset(entries[i].3 as usize);
                                                    // MRU: promote to front
                                                    if i > 0 {
                                                        entries.swap(0, i);
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

                        if let Some(val) = cached_val {
                            ctx.set_register(dst.0, val);
                            return Ok(());
                        }

                        // Slow path with IC update (skip for dictionary mode)
                        if !obj.is_dictionary_mode()
                            && let Some(offset) = obj.shape_get_offset(&key)
                        {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            let feedback = frame.feedback().write();
                            if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                                use otter_vm_bytecode::function::InlineCacheState;
                                let shape_ptr = obj.shape_id();
                                let current_epoch = ctx.cached_proto_epoch;

                                match &mut ic.ic_state {
                                    InlineCacheState::Uninitialized => {
                                        ic.ic_state = InlineCacheState::Monomorphic {
                                            shape_id: shape_ptr,
                                            proto_shape_id: 0,
                                            depth: 0,
                                            offset: offset as u32,
                                        };
                                        ic.proto_epoch = current_epoch;
                                    }
                                    InlineCacheState::Monomorphic {
                                        shape_id: old_shape,
                                        offset: old_offset,
                                        ..
                                    } => {
                                        if *old_shape != shape_ptr {
                                            let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                                            entries[0] = (*old_shape, 0, 0, *old_offset);
                                            entries[1] = (shape_ptr, 0, 0, offset as u32);
                                            ic.ic_state =
                                                InlineCacheState::Polymorphic { count: 2, entries };
                                            ic.proto_epoch = current_epoch;
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        let mut found = false;
                                        for entry in &entries[..(*count as usize)] {
                                            if entry.0 == shape_ptr {
                                                found = true;
                                                break;
                                            }
                                        }
                                        if !found {
                                            if (*count as usize) < 4 {
                                                entries[*count as usize] =
                                                    (shape_ptr, 0, 0, offset as u32);
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

                    let key_value = crate::proxy_operations::property_key_to_value_pub(&key);
                    let receiver = array;
                    let value = self.get_with_proxy_chain(ctx, &obj, &key, key_value, &receiver)?;
                    ctx.set_register(dst.0, value);
                } else {
                    ctx.set_register(dst.0, Value::undefined());
                }
                Ok(())
            }

            Instruction::SetElem {
                arr,
                idx,
                val,
                ic_index,
            } => {
                let array = *ctx.get_register(arr.0);
                let index = *ctx.get_register(idx.0);
                let val_val = *ctx.get_register(val.0);
                let is_strict = ctx
                    .current_frame()
                    .and_then(|frame| module.function(frame.function_index))
                    .map(|func| func.flags.is_strict)
                    .unwrap_or(false);

                // Proxy check - must be first
                if let Some(proxy) = array.as_proxy() {
                    let prop_key = self.value_to_property_key(ctx, &index)?;
                    let receiver = array;
                    {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_set(
                            &mut ncx, proxy, &prop_key, index, val_val, receiver,
                        )?;
                    }
                    return Ok(());
                }

                // TypedArray [[Set]] — §10.4.5.5
                // Per spec: for canonical numeric index, call ToNumber/ToBigInt(value),
                // write if buffer alive and index valid, always return true (no throw).
                if let Some(ta) = array.as_typed_array() {
                    match typed_array_ops::value_to_canonical_index(&index) {
                        Some(typed_array_ops::CanonicalIndex::Int(idx)) => {
                            // §10.4.5.11 IntegerIndexedElementSet: ToNumber/ToBigInt first
                            if ta.kind().is_bigint() {
                                let prim = if val_val.is_object() || val_val.as_object().is_some() {
                                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                                    ncx.to_primitive(&val_val, crate::interpreter::PreferredType::Number)?
                                } else {
                                    val_val
                                };
                                let n = crate::intrinsics_impl::typed_array::to_bigint_i64(&prim)?;
                                if !ta.is_detached() && idx < ta.length() {
                                    ta.set_bigint(idx, n);
                                }
                            } else {
                                let n = {
                                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                                    ncx.to_number_value(&val_val)?
                                };
                                if !ta.is_detached() && idx < ta.length() {
                                    ta.set(idx, n);
                                }
                            }
                            return Ok(());
                        }
                        Some(typed_array_ops::CanonicalIndex::NonInt) => {
                            // §10.4.5.11 IntegerIndexedElementSet: still call ToNumber/ToBigInt
                            // which can trigger side effects / throw
                            if ta.kind().is_bigint() {
                                let prim = if val_val.is_object() || val_val.as_object().is_some() {
                                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                                    ncx.to_primitive(&val_val, crate::interpreter::PreferredType::Number)?
                                } else {
                                    val_val
                                };
                                let _ = crate::intrinsics_impl::typed_array::to_bigint_i64(&prim)?;
                            } else {
                                let mut ncx = crate::context::NativeContext::new(ctx, self);
                                let _ = ncx.to_number_value(&val_val)?;
                            }
                            return Ok(());
                        }
                        None => {
                            // Not a numeric index — OrdinarySet on ta.object
                            let key = self.value_to_property_key(ctx, &index)?;
                            let _ = ta.object.set(key, val_val);
                            return Ok(());
                        }
                    }
                }

                if let Some(obj) = array.as_object() {
                    // Fast path for numeric index access on arrays
                    if obj.is_array()
                        && !obj.is_dictionary_mode()
                        && let Some(n) = index.as_int32()
                        && n >= 0
                        && obj.set_index(n as usize, val_val).is_ok()
                    {
                        return Ok(());
                    }

                    // Convert index to property key
                    let key = self.value_to_property_key(ctx, &index)?;

                    // IC Fast Path - only for string keys
                    if matches!(&key, PropertyKey::String(_)) {
                        let mut cached = false;
                        {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            let feedback = frame.feedback().write();
                            if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                                use otter_vm_bytecode::function::InlineCacheState;
                                let obj_shape_ptr = obj.shape_id();

                                if ic.proto_epoch_matches(ctx.cached_proto_epoch) {
                                    match &mut ic.ic_state {
                                        InlineCacheState::Monomorphic {
                                            shape_id, offset, ..
                                        } => {
                                            if obj_shape_ptr == *shape_id {
                                                match obj.set_by_offset(*offset as usize, val_val) {
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
                                                        entries[i].3 as usize,
                                                        val_val,
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
                                                    // MRU: promote to front
                                                    if i > 0 {
                                                        entries.swap(0, i);
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
                            return Ok(());
                        }

                        // Slow path with IC update (skip for dictionary mode)
                        if !obj.is_dictionary_mode()
                            && let Some(offset) = obj.shape_get_offset(&key)
                        {
                            let frame = ctx
                                .current_frame()
                                .ok_or_else(|| VmError::internal("no frame"))?;
                            let feedback = frame.feedback().write();
                            if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                                use otter_vm_bytecode::function::InlineCacheState;
                                let shape_ptr = obj.shape_id();
                                let current_epoch = ctx.cached_proto_epoch;

                                match &mut ic.ic_state {
                                    InlineCacheState::Uninitialized => {
                                        ic.ic_state = InlineCacheState::Monomorphic {
                                            shape_id: shape_ptr,
                                            proto_shape_id: 0,
                                            depth: 0,
                                            offset: offset as u32,
                                        };
                                        ic.proto_epoch = current_epoch;
                                    }
                                    InlineCacheState::Monomorphic {
                                        shape_id: old_shape,
                                        offset: old_offset,
                                        ..
                                    } => {
                                        if *old_shape != shape_ptr {
                                            let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                                            entries[0] = (*old_shape, 0, 0, *old_offset);
                                            entries[1] = (shape_ptr, 0, 0, offset as u32);
                                            ic.ic_state =
                                                InlineCacheState::Polymorphic { count: 2, entries };
                                            ic.proto_epoch = current_epoch;
                                        }
                                    }
                                    InlineCacheState::Polymorphic { count, entries } => {
                                        let mut found = false;
                                        for entry in &entries[..(*count as usize)] {
                                            if entry.0 == shape_ptr {
                                                found = true;
                                                break;
                                            }
                                        }
                                        if !found {
                                            if (*count as usize) < 4 {
                                                entries[*count as usize] =
                                                    (shape_ptr, 0, 0, offset as u32);
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

                    // §10.4.5.5: TypedArray in prototype chain intercepts numeric index
                    if let Some(ci) = typed_array_ops::canonical_numeric_index(&key) {
                        let mut proto_val = obj.prototype();
                        let mut found_ta = false;
                        let mut ta_valid_index = false;
                        for _ in 0..64 {
                            if proto_val.is_null() || proto_val.is_undefined() { break; }
                            if let Some(ta) = proto_val.as_typed_array() {
                                found_ta = true;
                                match ci {
                                    typed_array_ops::CanonicalIndex::Int(idx) => {
                                        if !ta.is_detached() && idx < ta.length() {
                                            ta_valid_index = true;
                                        }
                                    }
                                    typed_array_ops::CanonicalIndex::NonInt => {
                                        // NonInt → IsValidIntegerIndex always false
                                    }
                                }
                                break;
                            }
                            if let Some(p) = proto_val.as_object() {
                                proto_val = p.prototype();
                            } else { break; }
                        }
                        if found_ta && !ta_valid_index {
                            // §10.4.5.5 step 2.b.ii: invalid index → return true (do nothing)
                            return Ok(());
                        }
                        if ta_valid_index {
                            // Valid index, receiver !== TA → OrdinarySet on receiver
                            let success = if obj.is_array() {
                                if let PropertyKey::Index(i) = &key {
                                    obj.set_index(*i as usize, val_val).is_ok()
                                } else {
                                    obj.set(key, val_val).is_ok()
                                }
                            } else if !obj.is_extensible() {
                                false
                            } else {
                                obj.define_property(
                                    key,
                                    PropertyDescriptor::data_with_attrs(
                                        val_val,
                                        PropertyAttributes {
                                            writable: true,
                                            enumerable: true,
                                            configurable: true,
                                        },
                                    ),
                                )
                            };
                            if !success && is_strict {
                                return Err(VmError::type_error(
                                    "Cannot add property to a non-extensible object",
                                ));
                            }
                            return Ok(());
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
                                return Ok(());
                            };

                            if let Some(native_fn) = setter.as_native_function() {
                                self.call_native_fn(ctx, native_fn, &array, &[val_val])?;
                                return Ok(());
                            } else if let Some(closure) = setter.as_function() {
                                ctx.set_pending_args_one(val_val);
                                ctx.set_pending_this(array);
                                ctx.dispatch_action = Some(DispatchAction::Call {
                                    func_index: closure.function_index,
                                    module_id: closure.module.module_id,
                                    argc: 1,
                                    return_reg: 0, // Setter return value is ignored
                                    is_construct: false,
                                    is_async: closure.is_async,
                                    upvalues: closure.upvalues.clone(),
                                });
                                return Ok(());
                            } else {
                                return Err(VmError::type_error("setter is not a function"));
                            }
                        }
                        _ => {
                            if let Err(e) = obj.set(key, val_val) {
                                if matches!(e, SetPropertyError::InvalidArrayLength) {
                                    return Err(VmError::range_error(e.to_string()));
                                }
                                if is_strict {
                                    return Err(VmError::type_error(e.to_string()));
                                }
                            }
                        }
                    }
                } else if is_strict {
                    return Err(VmError::type_error("Cannot set property on non-object"));
                }
                Ok(())
            }

            Instruction::CallMethodComputed {
                dst,
                obj,
                key,
                argc,
                ic_index,
            } => {
                let receiver = *ctx.get_register(obj.0);
                let key_value = *ctx.get_register(key.0);

                // IC Fast Path
                // IC Fast Path
                let cached_method = if let Some(obj) = receiver.as_object() {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let feedback = frame.feedback().read();
                    if let Some(ic) = feedback.get(*ic_index as usize) {
                        if let otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                            shape_id: shape_addr,
                            offset,
                            ..
                        } = &ic.ic_state
                        {
                            if obj.shape_id() == *shape_addr {
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
                    if self.try_fast_path_array_method(
                        ctx,
                        &method_value,
                        &receiver,
                        *argc as u16,
                        obj.0 + 2,
                        dst.0,
                    )? {
                        return Ok(());
                    }

                    // Collect arguments (args start at obj + 2)
                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        args.push(*ctx.get_register(obj.0 + 2 + i));
                    }

                    // Direct call handling
                    return self.handle_call_value(ctx, &method_value, receiver, args, dst.0);
                }

                if let Some(proxy) = receiver.as_proxy() {
                    let prop_key = self.value_to_property_key(ctx, &key_value)?;
                    let method_value = {
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        crate::proxy_operations::proxy_get(
                            &mut ncx, proxy, &prop_key, key_value, receiver,
                        )?
                    };
                    if self.try_fast_path_array_method(
                        ctx,
                        &method_value,
                        &receiver,
                        *argc as u16,
                        obj.0 + 2,
                        dst.0,
                    )? {
                        return Ok(());
                    }

                    let mut args = Vec::with_capacity(*argc as usize);
                    for i in 0..(*argc as u16) {
                        args.push(*ctx.get_register(obj.0 + 2 + i));
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
                            Some(*ctx.get_register(key.0 + 1))
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
                                    generator.set_pending_throw(error_value);
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
                            return Ok(());
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
                        let result = GcRef::new(JsObject::new(Value::null()));
                        let _ = result.set(PropertyKey::string("value"), result_value);
                        let _ = result.set(PropertyKey::string("done"), Value::boolean(is_done));
                        ctx.set_register(dst.0, Value::object(result));
                        return Ok(());
                    }
                }

                let key = self.value_to_property_key(ctx, &key_value)?;
                let method_value = if let Some(obj_ref) = receiver.as_object() {
                    obj_ref.get(&key).unwrap_or_else(Value::undefined)
                } else if receiver.is_string() {
                    let proto = ctx
                        .string_prototype()
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
                if let Some(obj) = receiver.as_object()
                    && !obj.is_dictionary_mode()
                    && let Some(offset) = obj.shape_get_offset(&key)
                {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let feedback = frame.feedback().write();
                    if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                        use otter_vm_bytecode::function::InlineCacheState;
                        let shape_ptr = obj.shape_id();
                        let current_epoch = ctx.cached_proto_epoch;

                        match &mut ic.ic_state {
                            InlineCacheState::Uninitialized => {
                                ic.ic_state = InlineCacheState::Monomorphic {
                                    shape_id: shape_ptr,
                                    proto_shape_id: 0,
                                    depth: 0,
                                    offset: offset as u32,
                                };
                                ic.proto_epoch = current_epoch;
                            }
                            InlineCacheState::Monomorphic {
                                shape_id: old_shape,
                                offset: old_offset,
                                ..
                            } => {
                                if *old_shape != shape_ptr {
                                    let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                                    entries[0] = (*old_shape, 0, 0, *old_offset);
                                    entries[1] = (shape_ptr, 0, 0, offset as u32);
                                    ic.ic_state =
                                        InlineCacheState::Polymorphic { count: 2, entries };
                                    ic.proto_epoch = current_epoch;
                                }
                            }
                            InlineCacheState::Polymorphic { count, entries } => {
                                let mut found = false;
                                for entry in &entries[..(*count as usize)] {
                                    if entry.0 == shape_ptr {
                                        found = true;
                                        break;
                                    }
                                }
                                if !found {
                                    if (*count as usize) < 4 {
                                        entries[*count as usize] = (shape_ptr, 0, 0, offset as u32);
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

                if self.try_fast_path_array_method(
                    ctx,
                    &method_value,
                    &receiver,
                    *argc as u16,
                    obj.0 + 2,
                    dst.0,
                )? {
                    return Ok(());
                }

                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    args.push(*ctx.get_register(obj.0 + 2 + i));
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
                let receiver = *ctx.get_register(obj.0);
                let key_value = *ctx.get_register(key.0);
                let spread_arr = *ctx.get_register(spread.0);

                // IC Fast Path
                let cached_method = if let Some(obj) = receiver.as_object() {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let feedback = frame.feedback().read();
                    if let Some(ic) = feedback.get(*ic_index as usize) {
                        if let otter_vm_bytecode::function::InlineCacheState::Monomorphic {
                            shape_id: shape_addr,
                            offset,
                            ..
                        } = &ic.ic_state
                        {
                            if obj.shape_id() == *shape_addr {
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
                            &mut ncx, proxy, &prop_key, key_value, receiver,
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
                    let proto = ctx
                        .string_prototype()
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
                if let Some(obj) = receiver.as_object()
                    && !obj.is_dictionary_mode()
                    && let Some(offset) = obj.shape_get_offset(&key)
                {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let feedback = frame.feedback().write();
                    if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                        use otter_vm_bytecode::function::InlineCacheState;
                        let shape_ptr = obj.shape_id();
                        let current_epoch = ctx.cached_proto_epoch;

                        match &mut ic.ic_state {
                            InlineCacheState::Uninitialized => {
                                ic.ic_state = InlineCacheState::Monomorphic {
                                    shape_id: shape_ptr,
                                    proto_shape_id: 0,
                                    depth: 0,
                                    offset: offset as u32,
                                };
                                ic.proto_epoch = current_epoch;
                            }
                            InlineCacheState::Monomorphic {
                                shape_id: old_shape,
                                offset: old_offset,
                                ..
                            } => {
                                if *old_shape != shape_ptr {
                                    let mut entries = [(0u64, 0u64, 0u8, 0u32); 4];
                                    entries[0] = (*old_shape, 0, 0, *old_offset);
                                    entries[1] = (shape_ptr, 0, 0, offset as u32);
                                    ic.ic_state =
                                        InlineCacheState::Polymorphic { count: 2, entries };
                                    ic.proto_epoch = current_epoch;
                                }
                            }
                            InlineCacheState::Polymorphic { count, entries } => {
                                let mut found = false;
                                for entry in &entries[..(*count as usize)] {
                                    if entry.0 == shape_ptr {
                                        found = true;
                                        break;
                                    }
                                }
                                if !found {
                                    if (*count as usize) < 4 {
                                        entries[*count as usize] = (shape_ptr, 0, 0, offset as u32);
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

                self.dispatch_method_spread(ctx, &method_value, receiver, &spread_arr, dst.0)
            }

            Instruction::Spread { dst, src } => {
                // Spread elements from src array into dst array
                let dst_arr = *ctx.get_register(dst.0);
                let src_arr = *ctx.get_register(src.0);

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
                } else if let (Some(dst_obj), Some(src_str)) =
                    (dst_arr.as_object(), src_arr.as_string())
                {
                    // Spread string primitives by Unicode code point.
                    let mut out_index = dst_obj
                        .get(&PropertyKey::string("length"))
                        .and_then(|v| v.as_int32())
                        .unwrap_or(0) as u32;

                    let units = src_str.as_utf16();
                    let mut idx = 0usize;
                    while idx < units.len() {
                        let first = units[idx];
                        let (ch, next_idx) =
                            if crate::intrinsics_impl::string::is_high_surrogate(first)
                                && idx + 1 < units.len()
                                && crate::intrinsics_impl::string::is_low_surrogate(units[idx + 1])
                            {
                                (
                                    crate::string::JsString::intern_utf16(&[first, units[idx + 1]]),
                                    idx + 2,
                                )
                            } else {
                                (crate::string::JsString::intern_utf16(&[first]), idx + 1)
                            };

                        let _ = dst_obj.set(PropertyKey::Index(out_index), Value::string(ch));
                        out_index += 1;
                        idx = next_idx;
                    }

                    let _ = dst_obj.set(
                        PropertyKey::string("length"),
                        Value::int32(out_index as i32),
                    );
                }

                Ok(())
            }

            // ==================== Misc ====================
            Instruction::Move { dst, src } => {
                let value = *ctx.get_register(src.0);
                ctx.set_register(dst.0, value);
                Ok(())
            }

            Instruction::Nop => Ok(()),

            Instruction::Debugger => {
                ctx.trigger_debugger_hook();
                Ok(())
            }

            // ==================== Quickened Instructions ====================
            // Specialized variants created by bytecode quickening.
            // Each handler has a fast path + de-quicken fallback.
            Instruction::AddInt32 {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if let (Some(l), Some(r)) = (
                    ctx.get_register(lhs.0).as_int32(),
                    ctx.get_register(rhs.0).as_int32(),
                ) && let Some(result) = l.checked_add(r)
                {
                    ctx.set_register(dst.0, Value::int32(result));
                    return Ok(());
                }
                let left = *ctx.get_register(lhs.0);
                let right = *ctx.get_register(rhs.0);
                // De-quicken: revert to generic Add and execute generic path
                if let Some(frame) = ctx.current_frame()
                    && let Some(func) = module.function(frame.function_index)
                {
                    func.quicken_instruction(
                        frame.pc,
                        Instruction::Add {
                            dst: *dst,
                            lhs: *lhs,
                            rhs: *rhs,
                            feedback_index: *feedback_index,
                        },
                    );
                }
                let result = self.op_add(ctx, &left, &right)?;
                ctx.set_register(dst.0, result);
                Self::update_arithmetic_ic(ctx, *feedback_index, &left, &right);
                Ok(())
            }

            Instruction::SubInt32 {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if let (Some(l), Some(r)) = (
                    ctx.get_register(lhs.0).as_int32(),
                    ctx.get_register(rhs.0).as_int32(),
                ) && let Some(result) = l.checked_sub(r)
                {
                    ctx.set_register(dst.0, Value::int32(result));
                    return Ok(());
                }
                let left = *ctx.get_register(lhs.0);
                let right = *ctx.get_register(rhs.0);
                // De-quicken: revert to generic Sub
                if let Some(frame) = ctx.current_frame()
                    && let Some(func) = module.function(frame.function_index)
                {
                    func.quicken_instruction(
                        frame.pc,
                        Instruction::Sub {
                            dst: *dst,
                            lhs: *lhs,
                            rhs: *rhs,
                            feedback_index: *feedback_index,
                        },
                    );
                }
                let left_num = self.to_numeric(ctx, &left)?;
                let right_num = self.to_numeric(ctx, &right)?;
                match (left_num, right_num) {
                    (Numeric::BigInt(l), Numeric::BigInt(r)) => {
                        ctx.set_register(dst.0, Value::bigint((l - r).to_string()));
                    }
                    (Numeric::Number(l), Numeric::Number(r)) => {
                        ctx.set_register(dst.0, Value::number(l - r));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Self::update_arithmetic_ic(ctx, *feedback_index, &left, &right);
                Ok(())
            }

            Instruction::MulInt32 {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if let (Some(l), Some(r)) = (
                    ctx.get_register(lhs.0).as_int32(),
                    ctx.get_register(rhs.0).as_int32(),
                ) && let Some(result) = l.checked_mul(r)
                {
                    ctx.set_register(dst.0, Value::int32(result));
                    return Ok(());
                }
                let left = *ctx.get_register(lhs.0);
                let right = *ctx.get_register(rhs.0);
                // De-quicken: revert to generic Mul
                if let Some(frame) = ctx.current_frame()
                    && let Some(func) = module.function(frame.function_index)
                {
                    func.quicken_instruction(
                        frame.pc,
                        Instruction::Mul {
                            dst: *dst,
                            lhs: *lhs,
                            rhs: *rhs,
                            feedback_index: *feedback_index,
                        },
                    );
                }
                let left_num = self.to_numeric(ctx, &left)?;
                let right_num = self.to_numeric(ctx, &right)?;
                match (left_num, right_num) {
                    (Numeric::BigInt(l), Numeric::BigInt(r)) => {
                        ctx.set_register(dst.0, Value::bigint((l * r).to_string()));
                    }
                    (Numeric::Number(l), Numeric::Number(r)) => {
                        ctx.set_register(dst.0, Value::number(l * r));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Self::update_arithmetic_ic(ctx, *feedback_index, &left, &right);
                Ok(())
            }

            Instruction::DivInt32 {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if let (Some(l), Some(r)) = (
                    ctx.get_register(lhs.0).as_int32(),
                    ctx.get_register(rhs.0).as_int32(),
                ) && r != 0
                {
                    let (result, rem) = (l / r, l % r);
                    // Only use int32 fast path if division is exact (no remainder)
                    // and result doesn't lose precision (e.g., -2147483648 / -1)
                    if rem == 0 && !(l == i32::MIN && r == -1) {
                        ctx.set_register(dst.0, Value::int32(result));
                        return Ok(());
                    }
                }
                let left = *ctx.get_register(lhs.0);
                let right = *ctx.get_register(rhs.0);
                // De-quicken: revert to generic Div
                if let Some(frame) = ctx.current_frame()
                    && let Some(func) = module.function(frame.function_index)
                {
                    func.quicken_instruction(
                        frame.pc,
                        Instruction::Div {
                            dst: *dst,
                            lhs: *lhs,
                            rhs: *rhs,
                            feedback_index: *feedback_index,
                        },
                    );
                }
                let left_num = self.to_numeric(ctx, &left)?;
                let right_num = self.to_numeric(ctx, &right)?;
                match (left_num, right_num) {
                    (Numeric::BigInt(l), Numeric::BigInt(r)) => {
                        if r.is_zero() {
                            return Err(VmError::range_error("Division by zero"));
                        }
                        ctx.set_register(dst.0, Value::bigint((l / r).to_string()));
                    }
                    (Numeric::Number(l), Numeric::Number(r)) => {
                        ctx.set_register(dst.0, Value::number(l / r));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Self::update_arithmetic_ic(ctx, *feedback_index, &left, &right);
                Ok(())
            }

            Instruction::AddNumber {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if let (Some(l), Some(r)) = (
                    ctx.get_register(lhs.0).as_number(),
                    ctx.get_register(rhs.0).as_number(),
                ) {
                    ctx.set_register(dst.0, Value::number(l + r));
                    return Ok(());
                }
                let left = *ctx.get_register(lhs.0);
                let right = *ctx.get_register(rhs.0);
                // De-quicken: revert to generic Add
                if let Some(frame) = ctx.current_frame()
                    && let Some(func) = module.function(frame.function_index)
                {
                    func.quicken_instruction(
                        frame.pc,
                        Instruction::Add {
                            dst: *dst,
                            lhs: *lhs,
                            rhs: *rhs,
                            feedback_index: *feedback_index,
                        },
                    );
                }
                let result = self.op_add(ctx, &left, &right)?;
                ctx.set_register(dst.0, result);
                Self::update_arithmetic_ic(ctx, *feedback_index, &left, &right);
                Ok(())
            }

            Instruction::SubNumber {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if let (Some(l), Some(r)) = (
                    ctx.get_register(lhs.0).as_number(),
                    ctx.get_register(rhs.0).as_number(),
                ) {
                    ctx.set_register(dst.0, Value::number(l - r));
                    return Ok(());
                }
                let left = *ctx.get_register(lhs.0);
                let right = *ctx.get_register(rhs.0);
                // De-quicken: revert to generic Sub
                if let Some(frame) = ctx.current_frame()
                    && let Some(func) = module.function(frame.function_index)
                {
                    func.quicken_instruction(
                        frame.pc,
                        Instruction::Sub {
                            dst: *dst,
                            lhs: *lhs,
                            rhs: *rhs,
                            feedback_index: *feedback_index,
                        },
                    );
                }
                let left_num = self.to_numeric(ctx, &left)?;
                let right_num = self.to_numeric(ctx, &right)?;
                match (left_num, right_num) {
                    (Numeric::BigInt(l), Numeric::BigInt(r)) => {
                        ctx.set_register(dst.0, Value::bigint((l - r).to_string()));
                    }
                    (Numeric::Number(l), Numeric::Number(r)) => {
                        ctx.set_register(dst.0, Value::number(l - r));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Self::update_arithmetic_ic(ctx, *feedback_index, &left, &right);
                Ok(())
            }

            Instruction::GetPropQuickened {
                dst,
                obj,
                shape_id,
                offset,
                depth,
                proto_epoch,
                name,
                ic_index,
            } => {
                let object = ctx.get_register(obj.0);

                // Fast path: direct shape verify and load
                if let Some(obj_ref) = object.as_object() {
                    let obj_shape_ptr = obj_ref.shape_id();
                    if obj_shape_ptr == *shape_id {
                        if *depth == 0 {
                            // Own property: direct offset read
                            if let Some(val) = obj_ref.get_by_offset(*offset as usize) {
                                ctx.set_register(dst.0, val);
                                return Ok(());
                            }
                        } else if *proto_epoch == ctx.cached_proto_epoch {
                            // Inherited property: epoch guard confirms cached entry is valid.
                            // Walk to the prototype at depth and read the offset.
                            if let Some(val) = get_proto_value_at_depth(&obj_ref, *depth, *offset) {
                                ctx.set_register(dst.0, val);
                                return Ok(());
                            }
                        }
                        // else: epoch mismatch for depth>0, fall through to de-quicken
                    }
                }

                // Shape miss or epoch mismatch: de-quicken back to GetPropConst.
                if let Some(frame) = ctx.current_frame()
                    && let Some(func) = module.function(frame.function_index)
                {
                    func.quicken_instruction(
                        frame.pc,
                        Instruction::GetPropConst {
                            dst: *dst,
                            obj: *obj,
                            name: *name,
                            ic_index: *ic_index,
                        },
                    );
                }
                // Fall through to generic GetPropConst execution.
                let fallback = Instruction::GetPropConst {
                    dst: *dst,
                    obj: *obj,
                    name: *name,
                    ic_index: *ic_index,
                };
                self.execute_instruction(&fallback, module, ctx)
            }

            // Quickened: property access on a string primitive
            Instruction::GetPropString {
                dst,
                obj,
                name,
                ic_index,
            } => {
                let object = *ctx.get_register(obj.0);
                if object.as_string().is_some() {
                    let name_const = module
                        .constants
                        .get(name.0)
                        .ok_or_else(|| VmError::internal("constant not found"))?;
                    let name_str = name_const
                        .as_string()
                        .ok_or_else(|| VmError::internal("expected string constant"))?;
                    return self.handle_string_prop_access(ctx, &object, name_str, *dst);
                }
                // Type changed — de-quicken back to GetPropConst
                if let Some(frame) = ctx.current_frame()
                    && let Some(func) = module.function(frame.function_index)
                {
                    func.quicken_instruction(
                        frame.pc,
                        Instruction::GetPropConst {
                            dst: *dst,
                            obj: *obj,
                            name: *name,
                            ic_index: *ic_index,
                        },
                    );
                }
                let fallback = Instruction::GetPropConst {
                    dst: *dst,
                    obj: *obj,
                    name: *name,
                    ic_index: *ic_index,
                };
                self.execute_instruction(&fallback, module, ctx)
            }

            // Quickened: array .length fast access
            Instruction::GetArrayLength {
                dst,
                obj,
                name,
                ic_index,
            } => {
                let object = ctx.get_register(obj.0);
                if let Some(obj_ref) = object.as_object()
                    && obj_ref.is_array()
                {
                    ctx.set_register(dst.0, Value::int32(obj_ref.array_length() as i32));
                    return Ok(());
                }
                // Type changed — de-quicken back to GetPropConst
                if let Some(frame) = ctx.current_frame()
                    && let Some(func) = module.function(frame.function_index)
                {
                    func.quicken_instruction(
                        frame.pc,
                        Instruction::GetPropConst {
                            dst: *dst,
                            obj: *obj,
                            name: *name,
                            ic_index: *ic_index,
                        },
                    );
                }
                let fallback = Instruction::GetPropConst {
                    dst: *dst,
                    obj: *obj,
                    name: *name,
                    ic_index: *ic_index,
                };
                self.execute_instruction(&fallback, module, ctx)
            }

            // Superinstruction: fused GetLocal + GetPropConst
            Instruction::GetLocalProp {
                dst,
                local_idx,
                name,
                ic_index,
            } => {
                let object = ctx.get_local(local_idx.0)?;

                // Fast path: IC check on regular object (same as GetPropQuickened)
                if let Some(obj_ref) = object.as_object()
                    && !obj_ref.is_dictionary_mode()
                {
                    let frame = ctx
                        .current_frame()
                        .ok_or_else(|| VmError::internal("no frame"))?;
                    let feedback = frame.feedback().write();
                    if let Some(ic) = feedback.get_mut(*ic_index as usize) {
                        use otter_vm_bytecode::function::InlineCacheState;
                        let obj_shape_ptr = obj_ref.shape_id();
                        if ic.proto_epoch_matches(ctx.cached_proto_epoch) {
                            match &mut ic.ic_state {
                                InlineCacheState::Monomorphic {
                                    shape_id, offset, ..
                                } => {
                                    if obj_shape_ptr == *shape_id
                                        && let Some(val) = obj_ref.get_by_offset(*offset as usize)
                                    {
                                        ctx.set_register(dst.0, val);
                                        return Ok(());
                                    }
                                }
                                InlineCacheState::Polymorphic { count, entries } => {
                                    for i in 0..(*count as usize) {
                                        if obj_shape_ptr == entries[i].0 {
                                            if let Some(val) =
                                                obj_ref.get_by_offset(entries[i].3 as usize)
                                            {
                                                // MRU: promote to front
                                                if i > 0 {
                                                    entries.swap(0, i);
                                                }
                                                ctx.set_register(dst.0, val);
                                                return Ok(());
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

                // IC miss: store local into dst, then execute generic GetPropConst
                ctx.set_register(dst.0, object);
                let generic = Instruction::GetPropConst {
                    dst: *dst,
                    obj: *dst,
                    name: *name,
                    ic_index: *ic_index,
                };
                self.execute_instruction(&generic, module, ctx)
            }

            Instruction::SetPropQuickened {
                obj,
                val,
                shape_id,
                offset,
                name,
                ic_index,
            } => {
                let object = *ctx.get_register(obj.0);
                let value = *ctx.get_register(val.0);

                // Fast path: direct shape verify and store
                if let Some(obj_ref) = object.as_object() {
                    let obj_shape_ptr = obj_ref.shape_id();
                    if obj_shape_ptr == *shape_id
                        && obj_ref.set_by_offset(*offset as usize, value).is_ok()
                    {
                        return Ok(());
                    }
                }

                // Shape miss: de-quicken back to SetPropConst and execute the generic path.
                if let Some(frame) = ctx.current_frame()
                    && let Some(func) = module.function(frame.function_index)
                {
                    func.quicken_instruction(
                        frame.pc,
                        Instruction::SetPropConst {
                            obj: *obj,
                            name: *name,
                            val: *val,
                            ic_index: *ic_index,
                        },
                    );
                }
                let fallback = Instruction::SetPropConst {
                    obj: *obj,
                    name: *name,
                    val: *val,
                    ic_index: *ic_index,
                };
                self.execute_instruction(&fallback, module, ctx)
            }

            // ==================== Iteration ====================
            Instruction::GetIterator { dst, src } => {
                let obj = *ctx.get_register(src.0);

                // Get Symbol.iterator method
                let iterator_sym = crate::intrinsics::well_known::iterator_symbol();
                let iterator_method = if let Some(proxy) = obj.as_proxy() {
                    let key = PropertyKey::Symbol(iterator_sym);
                    let key_value = Value::symbol(iterator_sym);
                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                    Some(crate::proxy_operations::proxy_get(
                        &mut ncx, proxy, &key, key_value, obj,
                    )?)
                } else if obj.is_string() {
                    // String primitives: look up Symbol.iterator on String.prototype
                    let proto = ctx
                        .string_prototype()
                        .ok_or_else(|| VmError::type_error("String.prototype is not defined"))?;
                    proto.get(&PropertyKey::Symbol(iterator_sym))
                } else {
                    obj.as_object()
                        .and_then(|o| o.get(&PropertyKey::Symbol(iterator_sym)))
                };

                let iterator_fn =
                    iterator_method.ok_or_else(|| VmError::type_error("Object is not iterable"))?;

                // Call the iterator method with obj as `this`
                if let Some(native_fn) = iterator_fn.as_native_function() {
                    // Native iterator methods take the receiver as their first argument.
                    let iterator = self.call_native_fn(ctx, native_fn, &obj, &[])?;
                    ctx.set_register(dst.0, iterator);
                    Ok(())
                } else if let Some(closure) = iterator_fn.as_function() {
                    // JS iterator method: call with `this = obj` and no args.
                    ctx.set_pending_args_empty();
                    ctx.set_pending_this(obj);
                    ctx.dispatch_action = Some(DispatchAction::Call {
                        func_index: closure.function_index,
                        module_id: closure.module.module_id,
                        argc: 0,
                        return_reg: dst.0,
                        is_construct: false,
                        is_async: closure.is_async,
                        upvalues: closure.upvalues.clone(),
                    });
                    Ok(())
                } else {
                    Err(VmError::type_error("Symbol.iterator is not a function"))
                }
            }

            Instruction::GetAsyncIterator { dst, src } => {
                let obj = *ctx.get_register(src.0);

                // 1. Try Symbol.asyncIterator
                let async_iterator_sym = crate::intrinsics::well_known::async_iterator_symbol();
                let iterator_sym = crate::intrinsics::well_known::iterator_symbol();

                let mut iterator_method = if let Some(proxy) = obj.as_proxy() {
                    let key = PropertyKey::Symbol(async_iterator_sym);
                    let key_value = Value::symbol(async_iterator_sym);
                    let mut ncx = crate::context::NativeContext::new(ctx, self);
                    Some(crate::proxy_operations::proxy_get(
                        &mut ncx, proxy, &key, key_value, obj,
                    )?)
                } else {
                    obj.as_object()
                        .and_then(|o| o.get(&PropertyKey::Symbol(async_iterator_sym)))
                };

                // 2. Fallback to Symbol.iterator
                if iterator_method.is_none() {
                    if let Some(proxy) = obj.as_proxy() {
                        let key = PropertyKey::Symbol(iterator_sym);
                        let key_value = Value::symbol(iterator_sym);
                        let mut ncx = crate::context::NativeContext::new(ctx, self);
                        iterator_method = Some(crate::proxy_operations::proxy_get(
                            &mut ncx, proxy, &key, key_value, obj,
                        )?);
                    } else {
                        iterator_method = obj
                            .as_object()
                            .and_then(|o| o.get(&PropertyKey::Symbol(iterator_sym)));
                    }
                }

                let iterator_fn = iterator_method
                    .ok_or_else(|| VmError::type_error("Object is not async iterable"))?;

                // Call the iterator method with obj as `this`
                if let Some(native_fn) = iterator_fn.as_native_function() {
                    let iterator = self.call_native_fn(ctx, native_fn, &obj, &[])?;
                    // Per spec: If Type(iterator) is not Object, throw a TypeError
                    if !iterator.is_object() {
                        return Err(VmError::type_error(
                            "Result of the Symbol.asyncIterator method is not an object",
                        ));
                    }
                    ctx.set_register(dst.0, iterator);
                    Ok(())
                } else if let Some(closure) = iterator_fn.as_function() {
                    ctx.set_pending_args_empty();
                    ctx.set_pending_this(obj);
                    ctx.dispatch_action = Some(DispatchAction::Call {
                        func_index: closure.function_index,
                        module_id: closure.module.module_id,
                        argc: 0,
                        return_reg: dst.0,
                        is_construct: false,
                        is_async: closure.is_async,
                        upvalues: closure.upvalues.clone(),
                    });
                    Ok(())
                } else {
                    Err(VmError::type_error(
                        "Async iterator method is not a function",
                    ))
                }
            }

            Instruction::IteratorNext { dst, done, iter } => {
                let iterator = *ctx.get_register(iter.0);

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
                    return Ok(());
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
                Ok(())
            }

            Instruction::IteratorClose { iter } => {
                // Spec 7.4.6 IteratorClose
                let iterator = *ctx.get_register(iter.0);

                // 1. GetMethod(iterator, "return")
                let return_method = if let Some(obj) = iterator.as_object() {
                    obj.get(&PropertyKey::string("return"))
                        .unwrap_or(Value::undefined())
                } else {
                    Value::undefined()
                };

                // 2. If return is undefined or null, return (normal completion)
                if return_method.is_undefined() || return_method.is_null() {
                    return Ok(());
                }

                // 3. If not callable, throw TypeError
                if !return_method.is_callable() {
                    return Err(VmError::type_error("iterator.return is not a function"));
                }

                // 4. Call return method with iterator as this
                let inner_result = if let Some(native_fn) = return_method.as_native_function() {
                    self.call_native_fn(ctx, native_fn, &iterator, &[])?
                } else {
                    return Err(VmError::type_error("iterator.return is not a function"));
                };

                // 5. If result is not an object, throw TypeError
                if !inner_result.is_object() && inner_result.as_proxy().is_none() {
                    return Err(VmError::type_error("Iterator result is not an object"));
                }

                Ok(())
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
                let ctor_value = *ctx.get_register(ctor.0);
                let _mm = ctx.memory_manager().clone();

                if let Some(super_reg) = super_class {
                    // Derived class: set up prototype chain
                    let super_value = *ctx.get_register(super_reg.0);

                    // Validate superclass per spec 15.7.14 ClassDefinitionEvaluation:
                    // Step 5.e: if superclass === null (strict equality, NOT abstract)
                    // Step 5.f: if IsConstructor(superclass) is false, throw TypeError
                    if super_value.is_null() {
                        // extends null: create prototype with null __proto__
                        let derived_proto = GcRef::new(JsObject::new(Value::null()));

                        let proto_key = PropertyKey::string("prototype");
                        if let Some(ctor_obj) = ctor_value.as_object() {
                            let _ = ctor_obj.set(proto_key, Value::object(derived_proto));
                            let ctor_key = PropertyKey::string("constructor");
                            let _ = derived_proto.set(ctor_key, ctor_value);
                        }
                    } else {
                        // Step 5.f: IsConstructor check.
                        // In our VM, only HeapRef::Function closures (non-arrow, non-generator,
                        // non-async) are constructors. NativeFunction is NOT a constructor.
                        let is_constructor = if let Some(c) = super_value.as_function() {
                            let is_arrow = c
                                .module
                                .function(c.function_index)
                                .map(|f| f.is_arrow())
                                .unwrap_or(false);
                            !is_arrow && !c.is_generator && !c.is_async
                        } else if super_value.as_native_function().is_some() {
                            // Native functions can be constructors unless marked non-constructor
                            if let Some(obj) = super_value.as_object() {
                                if let Some(crate::object::PropertyDescriptor::Data {
                                    value, ..
                                }) = obj.get_own_property_descriptor(&PropertyKey::string(
                                    "__non_constructor",
                                )) {
                                    value.as_boolean() != Some(true)
                                } else {
                                    true
                                }
                            } else {
                                true
                            }
                        } else {
                            false
                        };

                        if !is_constructor {
                            return Err(VmError::TypeError(
                                "Class extends value is not a constructor or null".to_string(),
                            ));
                        }

                        // Get the underlying JsObject for property lookup
                        // (as_object() works for all HeapRef variants that represent objects)
                        let super_obj = super_value.as_object().ok_or_else(|| {
                            VmError::TypeError(
                                "Class extends value is not a constructor or null".to_string(),
                            )
                        })?;

                        // Get super.prototype
                        let proto_key = PropertyKey::string("prototype");
                        let super_proto_val =
                            super_obj.get(&proto_key).unwrap_or_else(Value::undefined);

                        // super.prototype must be object or null (spec step 5.g.ii)
                        let super_proto = if super_proto_val.is_null() {
                            None
                        } else if super_proto_val.is_object() {
                            // as_object() covers Function, NativeFunction, Array, etc.
                            super_proto_val.as_object()
                        } else if super_proto_val.is_undefined() {
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
                        ));

                        // Set ctor.prototype = derived_proto
                        if let Some(ctor_obj) = ctor_value.as_object() {
                            let _ = ctor_obj.set(
                                PropertyKey::string("prototype"),
                                Value::object(derived_proto),
                            );
                            let _ =
                                derived_proto.set(PropertyKey::string("constructor"), ctor_value);
                            // Static inheritance: ctor.__proto__ = super
                            // Preserve original HeapRef variant
                            ctor_obj.set_prototype(super_value);
                        }
                    }
                } else {
                    // Base class: ctor already has a .prototype from Closure creation
                    // Just ensure ctor.prototype.constructor = ctor
                    if let Some(ctor_obj) = ctor_value.as_object() {
                        let proto_key = PropertyKey::string("prototype");
                        if let Some(proto_val) = ctor_obj.get(&proto_key)
                            && let Some(proto_obj) = proto_val.as_object()
                        {
                            let _ = proto_obj.set(PropertyKey::string("constructor"), ctor_value);
                        }
                    }
                }

                ctx.set_register(dst.0, ctor_value);
                Ok(())
            }

            Instruction::CallSuper {
                dst,
                args: args_base,
                argc,
            } => {
                // Get the current frame's home_object and callee to find the superclass
                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame for CallSuper"))?;

                let home_object = frame.home_object.ok_or_else(|| {
                    VmError::ReferenceError("'super' keyword unexpected here".to_string())
                })?;

                // new_target_proto is the prototype for the object being created.
                // In the outermost derived constructor, this is home_object (e.g., C.prototype).
                // In deeper levels (multi-level), it was propagated from above.
                let new_target_proto = frame.new_target_proto.unwrap_or(home_object);

                // Per spec: GetSuperConstructor() = Object.getPrototypeOf(activeFunction)
                // The super constructor is found via the static inheritance chain of the
                // constructor function itself, NOT via the prototype chain of instances.
                // Use callee_value.__proto__ (set during Construct), falling back to the
                // old approach of home_object.__proto__.constructor for compatibility.
                let super_ctor_val = if let Some(callee) = frame.callee_value {
                    // Correct per spec: Object.getPrototypeOf(callee)
                    if let Some(callee_obj) = callee.as_object() {
                        callee_obj.prototype()
                    } else {
                        Value::undefined()
                    }
                } else {
                    // Fallback: walk through home_object.__proto__.constructor
                    let super_proto = home_object.prototype().as_object().ok_or_else(|| {
                        VmError::TypeError("Super constructor is not a constructor".to_string())
                    })?;
                    let ctor_key = PropertyKey::string("constructor");
                    super_proto.get(&ctor_key).unwrap_or_else(Value::undefined)
                };

                // Collect arguments from registers
                let mut args = Vec::with_capacity(*argc as usize);
                for i in 0..(*argc as u16) {
                    args.push(*ctx.get_register(args_base.0 + i));
                }
                let _mm = ctx.memory_manager().clone();

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
                        if let Some(proto_val) = super_closure.object.get(&proto_key)
                            && let Some(proto_obj) = proto_val.as_object()
                        {
                            ctx.set_pending_home_object(proto_obj);
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
                    let new_obj = GcRef::new(JsObject::new(Value::object(new_target_proto)));
                    let new_obj_value = Value::object(new_obj);

                    let result =
                        self.call_function_construct(ctx, &super_ctor_val, new_obj_value, &args)?;

                    // Native constructors may return a different object (e.g., Array creates a new array).
                    // Fix its prototype to new_target_proto for proper subclassing.

                    if result.is_object() {
                        if let Some(obj) = result.as_object() {
                            obj.set_prototype(Value::object(new_target_proto));
                        }
                        result
                    } else {
                        new_obj_value
                    }
                } else {
                    // Base case: super constructor is a regular (non-derived) closure.
                    let new_obj = GcRef::new(JsObject::new(Value::object(new_target_proto)));
                    let new_obj_value = Value::object(new_obj);

                    let result = self.call_function(ctx, &super_ctor_val, new_obj_value, &args)?;

                    if result.is_object() {
                        result
                    } else {
                        new_obj_value
                    }
                };

                // Set this_initialized and update this_value on current frame
                if let Some(frame) = ctx.current_frame_mut() {
                    frame.this_value = this_value;
                    frame.flags.set_this_initialized(true);
                }

                // Run field initializers (if any) now that `this` is ready
                self.run_field_initializers(ctx, &this_value)?;

                ctx.set_register(dst.0, this_value);
                Ok(())
            }

            Instruction::CallSuperForward { dst } => {
                // Default derived constructor: forward all arguments to super constructor.
                // Arguments are stored in locals (see push_frame extra_args handling).
                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame for CallSuperForward"))?;

                let home_object = frame.home_object.ok_or_else(|| {
                    VmError::ReferenceError("'super' keyword unexpected here".to_string())
                })?;
                let new_target_proto = frame.new_target_proto.unwrap_or(home_object);
                let argc = frame.argc as usize;

                // Collect arguments from locals (for empty default constructor, all args are extras at locals[0..argc])
                let mut args = Vec::with_capacity(argc);
                for i in 0..argc {
                    args.push(ctx.get_local(i as u16)?);
                }

                // Per spec: GetSuperConstructor() = Object.getPrototypeOf(activeFunction)
                let super_ctor_val = if let Some(callee) = frame.callee_value {
                    if let Some(callee_obj) = callee.as_object() {
                        callee_obj.prototype()
                    } else {
                        Value::undefined()
                    }
                } else {
                    let super_proto = home_object.prototype().as_object().ok_or_else(|| {
                        VmError::TypeError("Super constructor is not a constructor".to_string())
                    })?;
                    let ctor_key = PropertyKey::string("constructor");
                    super_proto.get(&ctor_key).unwrap_or_else(Value::undefined)
                };
                let _mm = ctx.memory_manager().clone();

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
                        if let Some(proto_val) = super_closure.object.get(&proto_key)
                            && let Some(proto_obj) = proto_val.as_object()
                        {
                            ctx.set_pending_home_object(proto_obj);
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
                    let new_obj = GcRef::new(JsObject::new(Value::object(new_target_proto)));
                    let new_obj_value = Value::object(new_obj);
                    let result =
                        self.call_function_construct(ctx, &super_ctor_val, new_obj_value, &args)?;
                    // Fix prototype for proper subclassing

                    if result.is_object() {
                        if let Some(obj) = result.as_object() {
                            obj.set_prototype(Value::object(new_target_proto));
                        }
                        result
                    } else {
                        new_obj_value
                    }
                } else {
                    let new_obj = GcRef::new(JsObject::new(Value::object(new_target_proto)));
                    let new_obj_value = Value::object(new_obj);
                    let result = self.call_function(ctx, &super_ctor_val, new_obj_value, &args)?;
                    if result.is_object() {
                        result
                    } else {
                        new_obj_value
                    }
                };

                if let Some(frame) = ctx.current_frame_mut() {
                    frame.this_value = this_value;
                    frame.flags.set_this_initialized(true);
                }

                // Run field initializers (if any) now that `this` is ready
                self.run_field_initializers(ctx, &this_value)?;

                ctx.set_register(dst.0, this_value);
                Ok(())
            }

            Instruction::CallSuperSpread { dst, args } => {
                // Like CallSuper but arguments come from a spread array
                let spread_arr = *ctx.get_register(args.0);

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

                let home_object = frame.home_object.ok_or_else(|| {
                    VmError::ReferenceError("'super' keyword unexpected here".to_string())
                })?;
                let new_target_proto = frame.new_target_proto.unwrap_or(home_object);

                // Per spec: GetSuperConstructor() = Object.getPrototypeOf(activeFunction)
                let super_ctor_val = if let Some(callee) = frame.callee_value {
                    if let Some(callee_obj) = callee.as_object() {
                        callee_obj.prototype()
                    } else {
                        Value::undefined()
                    }
                } else {
                    let super_proto = home_object.prototype().as_object().ok_or_else(|| {
                        VmError::TypeError("Super constructor is not a constructor".to_string())
                    })?;
                    let ctor_key = PropertyKey::string("constructor");
                    super_proto.get(&ctor_key).unwrap_or_else(Value::undefined)
                };
                let _mm = ctx.memory_manager().clone();

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
                        if let Some(proto_val) = super_closure.object.get(&proto_key)
                            && let Some(proto_obj) = proto_val.as_object()
                        {
                            ctx.set_pending_home_object(proto_obj);
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
                    let new_obj = GcRef::new(JsObject::new(Value::object(new_target_proto)));
                    let new_obj_value = Value::object(new_obj);
                    let result = self.call_function_construct(
                        ctx,
                        &super_ctor_val,
                        new_obj_value,
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
                    let new_obj = GcRef::new(JsObject::new(Value::object(new_target_proto)));
                    let new_obj_value = Value::object(new_obj);
                    let result =
                        self.call_function(ctx, &super_ctor_val, new_obj_value, &call_args)?;
                    if result.is_object() {
                        result
                    } else {
                        new_obj_value
                    }
                };

                if let Some(frame) = ctx.current_frame_mut() {
                    frame.this_value = this_value;
                    frame.flags.set_this_initialized(true);
                }

                // Run field initializers (if any) now that `this` is ready
                self.run_field_initializers(ctx, &this_value)?;

                ctx.set_register(dst.0, this_value);
                Ok(())
            }

            Instruction::GetSuper { dst } => {
                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame for GetSuper"))?;

                let home_object = frame.home_object.ok_or_else(|| {
                    VmError::ReferenceError("'super' keyword unexpected here".to_string())
                })?;

                // super = Object.getPrototypeOf(home_object)
                let result = home_object.prototype();

                ctx.set_register(dst.0, result);
                Ok(())
            }

            Instruction::GetSuperProp { dst, name } => {
                let frame = ctx
                    .current_frame()
                    .ok_or_else(|| VmError::internal("no frame for GetSuperProp"))?;

                let home_object = frame.home_object.ok_or_else(|| {
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
                    .map(|f| f.this_value)
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
                Ok(())
            }

            Instruction::SetHomeObject { func, obj } => {
                let func_val = *ctx.get_register(func.0);
                let obj_val = *ctx.get_register(obj.0);
                if let Some(closure) = func_val.as_function()
                    && let Some(obj_ref) = obj_val.as_object()
                {
                    // Create a new closure with home_object set
                    let new_closure = Closure {
                        function_index: closure.function_index,
                        module: Arc::clone(&closure.module),
                        upvalues: closure.upvalues.clone(),
                        is_async: closure.is_async,
                        is_generator: closure.is_generator,
                        object: closure.object,
                        home_object: Some(obj_ref),
                    };
                    ctx.set_register(func.0, Value::function(GcRef::new(new_closure)));
                }
                Ok(())
            }

            // ==================== Bitwise operators ====================
            Instruction::BitAnd { dst, lhs, rhs } => {
                if let (Some(l), Some(r)) = (
                    ctx.get_register(lhs.0).as_int32(),
                    ctx.get_register(rhs.0).as_int32(),
                ) {
                    ctx.set_register(dst.0, Value::int32(l & r));
                    return Ok(());
                }
                let l_val = *ctx.get_register(lhs.0);
                let r_val = *ctx.get_register(rhs.0);

                let l_numeric = self.to_numeric(ctx, &l_val)?;
                let r_numeric = self.to_numeric(ctx, &r_val)?;
                match (l_numeric, r_numeric) {
                    (Numeric::BigInt(l), Numeric::BigInt(r)) => {
                        ctx.set_register(dst.0, Value::bigint((l & r).to_string()));
                    }
                    (Numeric::Number(l), Numeric::Number(r)) => {
                        let l = self.to_int32_from(l);
                        let r = self.to_int32_from(r);
                        ctx.set_register(dst.0, Value::number((l & r) as f64));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Ok(())
            }
            Instruction::BitOr { dst, lhs, rhs } => {
                if let (Some(l), Some(r)) = (
                    ctx.get_register(lhs.0).as_int32(),
                    ctx.get_register(rhs.0).as_int32(),
                ) {
                    ctx.set_register(dst.0, Value::int32(l | r));
                    return Ok(());
                }
                let l_val = *ctx.get_register(lhs.0);
                let r_val = *ctx.get_register(rhs.0);

                let l_numeric = self.to_numeric(ctx, &l_val)?;
                let r_numeric = self.to_numeric(ctx, &r_val)?;
                match (l_numeric, r_numeric) {
                    (Numeric::BigInt(l), Numeric::BigInt(r)) => {
                        ctx.set_register(dst.0, Value::bigint((l | r).to_string()));
                    }
                    (Numeric::Number(l), Numeric::Number(r)) => {
                        let l = self.to_int32_from(l);
                        let r = self.to_int32_from(r);
                        ctx.set_register(dst.0, Value::number((l | r) as f64));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Ok(())
            }
            Instruction::BitXor { dst, lhs, rhs } => {
                if let (Some(l), Some(r)) = (
                    ctx.get_register(lhs.0).as_int32(),
                    ctx.get_register(rhs.0).as_int32(),
                ) {
                    ctx.set_register(dst.0, Value::int32(l ^ r));
                    return Ok(());
                }
                let l_val = *ctx.get_register(lhs.0);
                let r_val = *ctx.get_register(rhs.0);

                let l_numeric = self.to_numeric(ctx, &l_val)?;
                let r_numeric = self.to_numeric(ctx, &r_val)?;
                match (l_numeric, r_numeric) {
                    (Numeric::BigInt(l), Numeric::BigInt(r)) => {
                        ctx.set_register(dst.0, Value::bigint((l ^ r).to_string()));
                    }
                    (Numeric::Number(l), Numeric::Number(r)) => {
                        let l = self.to_int32_from(l);
                        let r = self.to_int32_from(r);
                        ctx.set_register(dst.0, Value::number((l ^ r) as f64));
                    }
                    _ => return Err(VmError::type_error("Cannot mix BigInt and other types")),
                }
                Ok(())
            }
            Instruction::BitNot { dst, src } => {
                if let Some(v) = ctx.get_register(src.0).as_int32() {
                    ctx.set_register(dst.0, Value::int32(!v));
                    return Ok(());
                }
                let v_val = *ctx.get_register(src.0);

                let numeric = self.to_numeric(ctx, &v_val)?;
                match numeric {
                    Numeric::BigInt(v) => {
                        ctx.set_register(dst.0, Value::bigint((!v).to_string()));
                    }
                    Numeric::Number(v) => {
                        let v = self.to_int32_from(v);
                        ctx.set_register(dst.0, Value::number((!v) as f64));
                    }
                }
                Ok(())
            }
            Instruction::Shl { dst, lhs, rhs } => {
                let l_val = ctx.get_register(lhs.0);
                let r_val = ctx.get_register(rhs.0);

                if let (Some(l), Some(r)) = (l_val.as_int32(), r_val.as_int32()) {
                    // `l` and `r` are signed, but JS shl treats shift amnt as u32 modulo 32
                    let shift = (r as u32) & 0x1f;
                    ctx.set_register(dst.0, Value::int32(l.wrapping_shl(shift)));
                    return Ok(());
                }

                let l_val_cloned = *l_val;
                let r_val_cloned = *r_val;
                let l = self.to_int32_from(self.coerce_number(ctx, l_val_cloned)?);
                let r = self.to_uint32_from(self.coerce_number(ctx, r_val_cloned)?);
                let shift = r & 0x1f;
                ctx.set_register(dst.0, Value::number((l.wrapping_shl(shift)) as f64));
                Ok(())
            }
            Instruction::Shr { dst, lhs, rhs } => {
                let l_val = ctx.get_register(lhs.0);
                let r_val = ctx.get_register(rhs.0);

                if let (Some(l), Some(r)) = (l_val.as_int32(), r_val.as_int32()) {
                    let shift = (r as u32) & 0x1f;
                    ctx.set_register(dst.0, Value::int32(l.wrapping_shr(shift)));
                    return Ok(());
                }

                let l_val_cloned = *l_val;
                let r_val_cloned = *r_val;
                let l = self.to_int32_from(self.coerce_number(ctx, l_val_cloned)?);
                let r = self.to_uint32_from(self.coerce_number(ctx, r_val_cloned)?);
                let shift = r & 0x1f;
                ctx.set_register(dst.0, Value::number((l.wrapping_shr(shift)) as f64));
                Ok(())
            }
            Instruction::Ushr { dst, lhs, rhs } => {
                let l_val = ctx.get_register(lhs.0);
                let r_val = ctx.get_register(rhs.0);

                if let (Some(l), Some(r)) = (l_val.as_int32(), r_val.as_int32()) {
                    let shift = (r as u32) & 0x1f;
                    // ushr converts left operand to an unsigned 32-bit int first
                    let result_u32 = (l as u32).wrapping_shr(shift);
                    // the result of >>> is always unsigned and must fit in a Number if >= 2^31
                    ctx.set_register(dst.0, Value::number(result_u32 as f64));
                    return Ok(());
                }

                let l_val_cloned = *l_val;
                let r_val_cloned = *r_val;
                let l = self.to_uint32_from(self.coerce_number(ctx, l_val_cloned)?);
                let r = self.to_uint32_from(self.coerce_number(ctx, r_val_cloned)?);
                let shift = r & 0x1f;
                ctx.set_register(dst.0, Value::number((l.wrapping_shr(shift)) as f64));
                Ok(())
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

                let js_regex =
                    GcRef::new(JsRegExp::new(pattern.to_string(), flags.to_string(), proto));
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
                    let mid = frame.module_id;
                    TemplateCacheKey {
                        realm_id: frame.realm_id,
                        module_ptr: Arc::as_ptr(ctx.module_table.get(mid)) as usize,
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
        let arr = GcRef::new(JsObject::array(values.len()));
        if let Some(array_obj) = ctx.get_global("Array").and_then(|v| v.as_object())
            && let Some(array_proto) = array_obj
                .get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object())
        {
            arr.set_prototype(Value::object(array_proto));
        }

        for (index, value) in values.iter().enumerate() {
            arr.set(PropertyKey::Index(index as u32), *value)
                .map_err(|e| VmError::internal(format!("failed to build template array: {e}")))?;
        }

        Ok(arr)
    }

    /// Determine the realm id for a constructor function (best-effort).
    pub(crate) fn realm_id_for_function(&self, ctx: &VmContext, value: &Value) -> RealmId {
        let mut current = *value;
        if let Some(proxy) = current.as_proxy()
            && let Some(target) = proxy.target()
        {
            current = target;
        }

        if let Some(obj) = current.as_object()
            && let Some(id) = obj
                .get(&PropertyKey::string("__realm_id__"))
                .and_then(|v| v.as_int32())
        {
            return id as RealmId;
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
            let mut current = *ctor;
            if let Some(proxy) = current.as_proxy()
                && let Some(target) = proxy.target()
            {
                current = target;
            }
            if let Some(tag) = current
                .as_object()
                .and_then(|o| o.get(&PropertyKey::string("__builtin_tag__")))
                .and_then(|v| v.as_string())
                && let Some(proto) = intrinsics.prototype_for_builtin_tag(tag.as_str())
            {
                return Some(proto);
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
                    *ctx.get_upvalue_cell(idx.0)?
                }
            };
            captured.push(cell);
        }

        Ok(captured)
    }

    /// Run the field initializer function for derived constructors after super() returns.
    /// The field_init_func is compiled as an inner function of the constructor.
    /// It is called with `this` bound to the newly created instance.
    pub fn run_field_initializers(&self, ctx: &mut VmContext, this_value: &Value) -> VmResult<()> {
        // Get the current frame's module and function index
        let (module, function_index) = {
            let frame = ctx
                .current_frame()
                .ok_or_else(|| VmError::internal("no frame for field init"))?;
            let mid = frame.module_id;
            let fidx = frame.function_index;
            (Arc::clone(ctx.module_table.get(mid)), fidx)
        };

        // Look up the constructor's field_init_func
        let field_init_idx = match module.function(function_index) {
            Some(func) => func.field_init_func,
            None => None,
        };

        if let Some(init_idx) = field_init_idx {
            // Get the field init function definition
            let init_func = module
                .function(init_idx)
                .ok_or_else(|| VmError::internal("field init function not found"))?;

            // Capture upvalues from the current frame (the constructor)
            let captured = self.capture_upvalues(ctx, &init_func.upvalues)?;

            // Create a minimal closure for the field init function
            let closure = GcRef::new(Closure {
                module: module.clone(),
                function_index: init_idx,
                upvalues: captured,
                object: GcRef::new(JsObject::new(Value::null())),
                is_async: false,
                is_generator: false,
                home_object: None,
            });

            let func_val = Value::function(closure);
            self.call_function(ctx, &func_val, *this_value, &[])?;
        }

        Ok(())
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
        let frames_array = GcRef::new(JsObject::array(frames.len()));

        for (i, frame) in frames.iter().enumerate() {
            let frame_obj = GcRef::new(JsObject::new(Value::null()));
            let frame_module = ctx.module_table.get(frame.module_id);

            // Get function name
            if let Some(func_def) = frame_module.functions.get(frame.function_index as usize) {
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
            let source_url = &frame_module.source_url;
            if !source_url.is_empty() {
                let _ = frame_obj.set(
                    PropertyKey::string("file"),
                    Value::string(JsString::intern(source_url)),
                );
            }

            // Resolve source location from function source map if present.
            // `frame.pc` can point at the next instruction, so also try `pc - 1`.
            if let Some(func) = frame_module.functions.get(frame.function_index as usize) {
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

mod generator;
pub use generator::GeneratorResult;
use generator::async_generator_result_to_promise_value;

#[cfg(test)]
mod tests;
