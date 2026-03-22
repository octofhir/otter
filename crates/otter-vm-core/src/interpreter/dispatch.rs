//! Unified dispatch helpers for DispatchAction::Call and TailCall.
//!
//! All run_loops delegate here instead of duplicating call/rest-array logic.
//! JIT check happens in ONE place — dispatch_call.

use super::*;

impl Interpreter {
    /// Handle DispatchAction::Call — the ONE dispatch point for all function calls.
    ///
    /// 1. Advance PC
    /// 2. Look up function, record call
    /// 3. Try JIT execution (if hot)
    /// 4. Handle rest params
    /// 5. Push interpreter frame
    pub(super) fn dispatch_call(
        &self,
        ctx: &mut VmContext,
        func_index: u32,
        module_id: u64,
        argc: u8,
        return_reg: u16,
        is_construct: bool,
        is_async: bool,
        upvalues: Vec<UpvalueCell>,
    ) -> VmResult<()> {
        ctx.advance_pc();

        // Extract function info (scoped borrow, no Arc clone)
        let (local_count, has_rest, param_count, func_ptr) = {
            let m = ctx.module_table.get(module_id);
            let f = m.function(func_index).ok_or_else(|| {
                VmError::internal(format!(
                    "callee not found (func_index={}, function_count={})",
                    func_index,
                    m.function_count()
                ))
            })?;
            f.record_call();
            (
                f.local_count,
                f.flags.has_rest,
                f.param_count as usize,
                f as *const otter_vm_bytecode::Function,
            )
        };

        // JIT: try compiled execution for hot functions
        if !is_construct {
            // SAFETY: func_ptr is valid — it points into Arc<Module> which is
            // alive in ctx.module_table for the entire VM lifetime.
            let f = unsafe { &*func_ptr };
            if Self::can_jit(f, is_construct, is_async, argc) {
                match self.try_jit_call(ctx, f, module_id, &upvalues) {
                    JitOutcome::Executed(value) => {
                        ctx.set_register(return_reg, value);
                        let _ = ctx.take_pending_args();
                        return Ok(());
                    }
                    JitOutcome::Fallthrough => {}
                }
            }
        }

        if has_rest {
            self.setup_rest_params(ctx, param_count);
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
        )
    }

    /// Handle DispatchAction::TailCall.
    ///
    /// Caller must call `ctx.pop_frame_discard()` before invoking this.
    pub(super) fn dispatch_tail_call(
        &self,
        ctx: &mut VmContext,
        func_index: u32,
        module_id: u64,
        argc: u8,
        return_reg: u16,
        is_async: bool,
        upvalues: Vec<UpvalueCell>,
    ) -> VmResult<()> {
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
            self.setup_rest_params(ctx, param_count);
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
        )
    }

    /// Construct rest array from excess pending args.
    pub(super) fn setup_rest_params(&self, ctx: &mut VmContext, param_count: usize) {
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
}

/// Outcome of a JIT execution attempt.
enum JitOutcome {
    /// JIT ran successfully, here's the return value.
    Executed(Value),
    /// JIT unavailable or bailed out, fall through to interpreter.
    Fallthrough,
}

impl Interpreter {
    /// Try to execute a function via the new otter-jit pipeline.
    ///
    /// Called from dispatch_call when can_jit is true.
    fn try_jit_call(
        &self,
        ctx: &mut VmContext,
        func: &otter_vm_bytecode::Function,
        module_id: u64,
        upvalues: &[UpvalueCell],
    ) -> JitOutcome {
        // Snapshot values before mutable borrow
        let this_val = ctx
            .pending_this_to_trace()
            .cloned()
            .unwrap_or_else(Value::undefined);
        let callee_raw = ctx
            .pending_callee_to_trace()
            .map(|v| v.to_jit_bits() as u64)
            .unwrap_or(otter_jit::codegen::value_repr::TAG_UNDEFINED);
        let home_obj_raw = ctx
            .pending_home_object_to_trace()
            .map(|ho| Value::object(*ho).to_jit_bits() as u64)
            .unwrap_or(otter_jit::codegen::value_repr::TAG_UNDEFINED);
        let reg_base = ctx.current_register_base();
        let epoch = ctx.cached_proto_epoch;

        // Get constant pool pointer (scoped borrow)
        let const_ptr = {
            let m = ctx.module_table.get(module_id);
            &m.constants as *const _ as *const otter_vm_bytecode::ConstantPool
        };

        // Now safe to take mutable borrow for registers
        let regs_ptr = ctx.registers_mut_ptr();

        match crate::jit_runtime::try_execute_jit(
            func,
            regs_ptr,
            reg_base,
            this_val,
            const_ptr,
            self as *const _ as *const crate::interpreter::Interpreter,
            ctx as *mut _ as *mut crate::context::VmContext,
            upvalues,
            callee_raw,
            home_obj_raw,
            epoch,
            std::ptr::null(),
        ) {
            crate::jit_runtime::JitCallResult::Ok(value) => JitOutcome::Executed(value),
            crate::jit_runtime::JitCallResult::Bailout { .. }
            | crate::jit_runtime::JitCallResult::NotCompiled => JitOutcome::Fallthrough,
        }
    }
}
