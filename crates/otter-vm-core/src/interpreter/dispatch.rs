//! Unified dispatch helpers for DispatchAction::Call and TailCall.
//!
//! All run_loops delegate here instead of duplicating call/rest-array logic.
//! JIT check happens in ONE place — dispatch_call, AFTER push_frame.

use super::*;

/// Outcome of a JIT execution attempt.
enum JitOutcome {
    /// JIT completed, here's the return value.
    Executed(Value),
    /// JIT bailed out, interpreter should resume at this bytecode PC.
    BailoutResume { bytecode_pc: u32 },
    /// JIT not available, interpret normally.
    Fallthrough,
}

impl Interpreter {
    /// Handle DispatchAction::Call — the ONE dispatch point for all function calls.
    ///
    /// Flow:
    /// 1. Advance caller's PC past the Call instruction
    /// 2. Look up function, record call
    /// 3. Handle rest params
    /// 4. Push interpreter frame (initializes locals from pending_args)
    /// 5. Try JIT on the now-active frame
    /// 6. On JIT success → pop frame, write result to caller
    /// 7. On JIT bailout → set PC, interpreter resumes from bailout point
    /// 8. On fallthrough → interpreter runs from PC 0
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

        // Extract function info (scoped borrow, no Arc clone).
        let (local_count, has_rest, param_count) = {
            let m = ctx.module_table.get(module_id);
            let f = m.function(func_index).ok_or_else(|| {
                VmError::internal(format!(
                    "callee not found (func_index={}, function_count={})",
                    func_index,
                    m.function_count()
                ))
            })?;
            f.record_call();
            (f.local_count, f.flags.has_rest, f.param_count as usize)
        };

        // Handle rest parameters (modifies pending_args before push_frame consumes them).
        if has_rest {
            self.setup_rest_params(ctx, param_count);
        }

        // Push the frame. This consumes pending_args into local slots,
        // sets up the register window, and makes the callee the current frame.
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

        // Now the frame is live. Locals are initialized from args.
        // Try JIT execution on the fully set-up frame.
        if !is_construct && !is_async {
            let jit_eligible = {
                let m = ctx.module_table.get(module_id);
                let f = m.function(func_index).unwrap();
                Self::can_jit(f, is_construct, is_async, argc)
            };

            if jit_eligible {
                match self.try_jit_call(ctx, module_id, func_index) {
                    JitOutcome::Executed(value) => {
                        // JIT ran successfully. Pop the callee frame
                        // and write result into the caller's return register.
                        ctx.pop_frame_discard();
                        ctx.set_register(return_reg, value);
                        return Ok(());
                    }
                    JitOutcome::BailoutResume { bytecode_pc } => {
                        // JIT bailed out. Frame stays pushed.
                        // Set PC to bailout point; interpreter resumes there.
                        if let Some(frame) = ctx.current_frame_mut() {
                            frame.pc = bytecode_pc as usize;
                        }
                        return Ok(());
                    }
                    JitOutcome::Fallthrough => {
                        // JIT unavailable. Interpreter runs from PC 0.
                    }
                }
            }
        }

        Ok(())
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

    /// Try to execute a function via JIT on the CURRENT (already pushed) frame.
    ///
    /// The frame's locals are already initialized from args.
    /// register_base points to the callee's window.
    fn try_jit_call(
        &self,
        ctx: &mut VmContext,
        module_id: u64,
        func_index: u32,
    ) -> JitOutcome {
        // Read frame state — frame is already pushed and active.
        let this_val = ctx.current_frame()
            .map(|f| f.this_value)
            .unwrap_or_else(Value::undefined);
        let callee_raw = ctx.current_frame()
            .and_then(|f| f.callee_value)
            .map(|v| v.to_jit_bits() as u64)
            .unwrap_or(otter_jit::codegen::value_repr::TAG_UNDEFINED);
        let home_obj_raw = ctx.current_frame()
            .and_then(|f| f.home_object)
            .map(|ho| Value::object(ho).to_jit_bits() as u64)
            .unwrap_or(otter_jit::codegen::value_repr::TAG_UNDEFINED);
        let reg_base = ctx.current_register_base();
        let epoch = ctx.cached_proto_epoch;

        // Get function and constant pool (scoped borrow).
        let func_ptr: *const otter_vm_bytecode::Function;
        let const_ptr: *const otter_vm_bytecode::ConstantPool;
        {
            let m = ctx.module_table.get(module_id);
            let f = m.function(func_index).unwrap();
            func_ptr = f as *const _;
            const_ptr = &m.constants as *const _;
        }

        // Get upvalues from the current frame.
        let upvalues_ptr: *const ();
        let upvalue_count: u32;
        {
            let frame = ctx.current_frame().unwrap();
            if frame.upvalues.is_empty() {
                upvalues_ptr = std::ptr::null();
                upvalue_count = 0;
            } else {
                upvalues_ptr = frame.upvalues.as_ptr() as *const ();
                upvalue_count = frame.upvalues.len() as u32;
            }
        }

        // Now safe to take mutable borrow for registers.
        let regs_ptr = ctx.registers_mut_ptr();
        let func = unsafe { &*func_ptr };

        match crate::jit_runtime::try_execute_jit(
            func,
            regs_ptr,
            reg_base,
            this_val,
            const_ptr,
            self as *const _ as *const crate::interpreter::Interpreter,
            ctx as *mut _ as *mut crate::context::VmContext,
            // Pass empty upvalues slice — upvalues_ptr/count are in JitContext directly.
            &[],
            callee_raw,
            home_obj_raw,
            epoch,
            std::ptr::null(),
        ) {
            crate::jit_runtime::JitCallResult::Ok(value) => JitOutcome::Executed(value),
            crate::jit_runtime::JitCallResult::Bailout { bytecode_pc } => {
                JitOutcome::BailoutResume { bytecode_pc }
            }
            crate::jit_runtime::JitCallResult::NotCompiled => JitOutcome::Fallthrough,
        }
    }
}
