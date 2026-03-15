use super::*;

pub(super) enum BackEdgeOsrOutcome {
    ContinueWithJump,
    ContinueAtDeoptPc,
    Returned(Value),
}

impl Interpreter {
    #[inline]
    pub(super) fn get_arithmetic_fast_path(
        ctx: &VmContext,
        feedback_index: u16,
    ) -> Option<otter_vm_bytecode::function::ArithmeticType> {
        if let Some(frame) = ctx.current_frame() {
            let feedback = frame.feedback().read();
            if let Some(ic) = feedback.get(feedback_index as usize)
                && let otter_vm_bytecode::function::InlineCacheState::ArithmeticFastPath(ty) =
                    ic.ic_state
            {
                return Some(ty);
            }
        }
        None
    }

    #[inline]
    pub(super) fn update_arithmetic_ic(
        ctx: &VmContext,
        feedback_index: u16,
        left: &Value,
        right: &Value,
    ) {
        if let Some(frame) = ctx.current_frame() {
            let feedback = frame.feedback().write();
            if let Some(ic) = feedback.get_mut(feedback_index as usize) {
                Self::observe_value_type(&mut ic.type_observations, left);
                Self::observe_value_type(&mut ic.type_observations, right);

                let new_ty = if ic.type_observations.is_int32_only() {
                    Some(otter_vm_bytecode::function::ArithmeticType::Int32)
                } else if ic.type_observations.is_numeric_only() {
                    Some(otter_vm_bytecode::function::ArithmeticType::Number)
                } else if !ic.type_observations.seen_object
                    && !ic.type_observations.seen_function
                    && ic.type_observations.seen_string
                {
                    Some(otter_vm_bytecode::function::ArithmeticType::String)
                } else {
                    None
                };

                if let Some(ty) = new_ty {
                    ic.ic_state =
                        otter_vm_bytecode::function::InlineCacheState::ArithmeticFastPath(ty);
                } else {
                    ic.ic_state = otter_vm_bytecode::function::InlineCacheState::Megamorphic;
                }

                // Quickening: after enough consistent observations, specialize the instruction
                ic.hit_count = ic.hit_count.saturating_add(1);
                if ic.hit_count >= otter_vm_bytecode::function::QUICKENING_WARMUP {
                    let frame_module = ctx.module_table.get(frame.module_id);
                    if let Some(func) = frame_module.function(frame.function_index) {
                        let pc = frame.pc;
                        let instr = &func.instructions.read()[pc];
                        Self::try_quicken_arithmetic(func, pc, instr, &ic.type_observations);
                    }
                }
            }
        }
    }

    /// Check if a function is eligible for JIT compilation.
    #[inline]
    pub(super) fn can_jit(
        func: &otter_vm_bytecode::Function,
        is_construct: bool,
        is_async: bool,
        argc: u8,
    ) -> bool {
        otter_vm_exec::is_jit_enabled()
            && func.is_hot_function()
            && !func.is_deoptimized()
            && !is_construct
            && !is_async
            && !func.flags.has_rest
            && !func.flags.uses_arguments
            && !func.flags.uses_eval
            && argc <= func.param_count
    }

    /// Attempt to quicken a property access instruction based on IC state.
    #[inline]
    pub(super) fn try_quicken_property_access(
        func: &otter_vm_bytecode::Function,
        pc: usize,
        instruction: &Instruction,
        shape_id: u64,
        offset: u32,
        depth: u8,
        proto_epoch: u64,
    ) {
        match instruction {
            Instruction::GetPropConst {
                dst,
                obj,
                name,
                ic_index,
            } => {
                func.quicken_instruction(
                    pc,
                    Instruction::GetPropQuickened {
                        dst: *dst,
                        obj: *obj,
                        shape_id,
                        offset,
                        depth,
                        proto_epoch,
                        name: *name,
                        ic_index: *ic_index,
                    },
                );
            }
            Instruction::SetPropConst {
                obj,
                name,
                val,
                ic_index,
            } => {
                func.quicken_instruction(
                    pc,
                    Instruction::SetPropQuickened {
                        obj: *obj,
                        val: *val,
                        shape_id,
                        offset,
                        name: *name,
                        ic_index: *ic_index,
                    },
                );
            }
            _ => {}
        }
    }

    /// Attempt to quicken an arithmetic instruction based on type observations.
    #[inline]
    pub(super) fn try_quicken_arithmetic(
        func: &otter_vm_bytecode::Function,
        pc: usize,
        instruction: &Instruction,
        observations: &otter_vm_bytecode::function::TypeFlags,
    ) {
        let quickened = match instruction {
            Instruction::Add {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if observations.is_int32_only() {
                    Some(Instruction::AddInt32 {
                        dst: *dst,
                        lhs: *lhs,
                        rhs: *rhs,
                        feedback_index: *feedback_index,
                    })
                } else if observations.is_numeric_only() {
                    Some(Instruction::AddNumber {
                        dst: *dst,
                        lhs: *lhs,
                        rhs: *rhs,
                        feedback_index: *feedback_index,
                    })
                } else {
                    None
                }
            }
            Instruction::Sub {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if observations.is_int32_only() {
                    Some(Instruction::SubInt32 {
                        dst: *dst,
                        lhs: *lhs,
                        rhs: *rhs,
                        feedback_index: *feedback_index,
                    })
                } else if observations.is_numeric_only() {
                    Some(Instruction::SubNumber {
                        dst: *dst,
                        lhs: *lhs,
                        rhs: *rhs,
                        feedback_index: *feedback_index,
                    })
                } else {
                    None
                }
            }
            Instruction::Mul {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if observations.is_int32_only() {
                    Some(Instruction::MulInt32 {
                        dst: *dst,
                        lhs: *lhs,
                        rhs: *rhs,
                        feedback_index: *feedback_index,
                    })
                } else {
                    None
                }
            }
            Instruction::Div {
                dst,
                lhs,
                rhs,
                feedback_index,
            } => {
                if observations.is_int32_only() {
                    Some(Instruction::DivInt32 {
                        dst: *dst,
                        lhs: *lhs,
                        rhs: *rhs,
                        feedback_index: *feedback_index,
                    })
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(new_instr) = quickened {
            func.quicken_instruction(pc, new_instr);
        }
    }
    #[cfg(test)]
    pub(super) fn has_backward_jump(function: &otter_vm_bytecode::Function) -> bool {
        function
            .instructions
            .read()
            .iter()
            .any(|instruction| match instruction {
                Instruction::Jump { offset }
                | Instruction::JumpIfTrue { offset, .. }
                | Instruction::JumpIfFalse { offset, .. }
                | Instruction::JumpIfNullish { offset, .. }
                | Instruction::JumpIfNotNullish { offset, .. }
                | Instruction::ForInNext { offset, .. } => offset.0 < 0,
                _ => false,
            })
    }
    #[cfg(test)]
    pub(super) fn is_static_jit_candidate(function: &otter_vm_bytecode::Function) -> bool {
        !function.flags.is_async
            && !function.flags.has_rest
            && !function.flags.uses_arguments
            && !function.flags.uses_eval
    }
    /// Handle a backward jump (back-edge) for loop-hot function detection and OSR.
    ///
    /// **Stage 1 — Back-edge counting:** Increments the function's back-edge
    /// counter. When the counter crosses the hot threshold, the function is
    /// marked hot and synchronously compiled by the JIT.
    ///
    /// **Stage 2 — True OSR:** If JIT code is already available, enters JIT at
    /// the loop header with the interpreter's full frame state (all locals +
    /// registers). Unlike the old full-restart approach, setup code before the
    /// loop is NOT re-executed.
    ///
    /// `target_pc` is the bytecode PC of the backward jump target (loop header).
    ///
    /// Returns:
    /// - `Returned(value)` if OSR completed the function in JIT
    /// - `ContinueAtDeoptPc` when JIT bailed out with precise resume state
    /// - `ContinueWithJump` for normal interpreter jump handling
    #[inline]
    pub(super) fn try_back_edge_osr(
        &self,
        ctx: &mut VmContext,
        module: &Arc<Module>,
        func: &otter_vm_bytecode::Function,
        target_pc: usize,
    ) -> BackEdgeOsrOutcome {
        // ---- Stage 1: back-edge counting + JIT enqueue ----
        let newly_hot = func.record_back_edge_with_threshold(otter_vm_exec::jit_hot_threshold());
        if newly_hot {
            func.mark_hot();
            if otter_vm_exec::is_jit_enabled() {
                let Some(frame) = ctx.current_frame() else {
                    return BackEdgeOsrOutcome::ContinueWithJump;
                };
                let func_index = frame.function_index;
                otter_vm_exec::enqueue_hot_function(module, func_index, func);
                otter_vm_exec::compile_one_pending_request(crate::jit_runtime::runtime_helpers());
                otter_vm_exec::record_back_edge_compilation();
            }
        }

        // ---- Stage 2: True OSR at loop header ----
        if !otter_vm_exec::is_jit_enabled()
            || !func.is_hot_function()
            || func.is_deoptimized()
            || func.flags.has_rest
            || func.flags.uses_arguments
            || func.flags.uses_eval
        {
            return BackEdgeOsrOutcome::ContinueWithJump;
        }

        // Extract all needed state from the frame in one borrow.
        let Some(frame) = ctx.current_frame() else {
            return BackEdgeOsrOutcome::ContinueWithJump;
        };
        if frame.flags.is_construct() || frame.flags.is_async() {
            return BackEdgeOsrOutcome::ContinueWithJump;
        }
        let func_index = frame.function_index;
        let this_value = frame.this_value;
        let home_object = frame.home_object;
        let upvalues = frame.upvalues.clone();
        // frame borrow ends here

        // IC-driven recompilation: if a JIT helper detected IC transitions
        // (Uninitialized → Monomorphic) after the function was compiled,
        // it cleared the JIT entry and set the recompilation flag. Recompile
        // synchronously (not via background worker) so the result is available
        // for immediate OSR re-entry.
        if func.take_ic_recompilation_needed() && otter_vm_exec::is_jit_enabled() {
            otter_vm_exec::enqueue_hot_function(module, func_index, func);
            otter_vm_exec::compile_one_pending_request_sync(crate::jit_runtime::runtime_helpers());
        }

        // Background JIT may have compiled code in the runtime cache while
        // function-local entry pointer is still not populated.
        if !otter_vm_exec::hydrate_jit_entry_ptr(module.module_id, func_index, func) {
            return BackEdgeOsrOutcome::ContinueWithJump;
        }

        // Extract ALL locals and registers for true OSR.
        let local_count = func.local_count as usize;
        let reg_count = func.register_count as usize;
        let locals: Vec<Value> = (0..local_count)
            .map(|i| {
                ctx.get_local(i as u16)
                    .unwrap_or_else(|_| Value::undefined())
            })
            .collect();
        let registers: Vec<Value> = (0..reg_count)
            .map(|i| *ctx.get_register(i as u16))
            .collect();

        // Build args from parameter locals (for JIT context argv, though OSR
        // loads from deopt buffers instead).
        let param_count = func.param_count as usize;
        let Some(frame) = ctx.current_frame() else {
            return BackEdgeOsrOutcome::ContinueWithJump;
        };
        let argc = param_count.min(frame.argc as usize);
        let args: Vec<Value> = (0..argc)
            .map(|i| {
                ctx.get_local(i as u16)
                    .unwrap_or_else(|_| Value::undefined())
            })
            .collect();

        // Set up pending state for the JIT context.
        ctx.set_pending_this(this_value);
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
                BackEdgeOsrOutcome::Returned(value)
            }
            crate::jit_runtime::JitCallResult::BailoutResume(state) => {
                crate::jit_resume::resume_in_place(ctx, &state);
                BackEdgeOsrOutcome::ContinueAtDeoptPc
            }
            crate::jit_runtime::JitCallResult::NeedsRecompilation => {
                otter_vm_exec::enqueue_hot_function(module, func_index, func);
                otter_vm_exec::compile_one_pending_request(crate::jit_runtime::runtime_helpers());
                BackEdgeOsrOutcome::ContinueWithJump
            }
            _ => BackEdgeOsrOutcome::ContinueWithJump,
        }
    }
}
