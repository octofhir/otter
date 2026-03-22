//! Interpreter ↔ JIT bridge — stub during JIT rebuild.
//!
//! IC updates and quickening remain functional (they're interpreter features).
//! JIT compilation and execution are disabled pending new otter-jit integration.

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
    pub(crate) fn update_arithmetic_ic(
        ctx: &VmContext,
        feedback_index: u16,
        left: &Value,
        right: &Value,
    ) {
        if let Some(frame) = ctx.current_frame() {
            let function = ctx
                .module_table
                .get(frame.module_id)
                .function(frame.function_index)
                .unwrap();
            Self::update_arithmetic_ic_on_function(
                ctx,
                function,
                feedback_index,
                left,
                right,
                Some(frame.pc),
            );
        }
    }

    pub(crate) fn update_arithmetic_ic_on_function(
        _ctx: &VmContext,
        func: &otter_vm_bytecode::Function,
        feedback_index: u16,
        left: &Value,
        right: &Value,
        pc: Option<usize>,
    ) {
        let mut feedback = func.feedback_vector.write();
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

            // Quickening
            ic.hit_count = ic.hit_count.saturating_add(1);
            if ic.hit_count >= otter_vm_bytecode::function::QUICKENING_WARMUP {
                if let Some(pc) = pc {
                    let instr = &func.instructions.read()[pc];
                    Self::try_quicken_arithmetic(func, pc, instr, &ic.type_observations);
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
        _argc: u8,
    ) -> bool {
        otter_jit::pipeline::should_jit(func)
            && !is_construct
            && !is_async
    }

    /// Property access quickening (interpreter feature, not JIT).
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

    /// Arithmetic quickening (interpreter feature, not JIT).
    #[inline]
    pub(crate) fn try_quicken_arithmetic(
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

        if let Some(q) = quickened {
            func.quicken_instruction(pc, q);
        }
    }

    /// OSR disabled during JIT rebuild. Still records back-edge heat for profiling.
    pub(super) fn try_back_edge_osr(
        &self,
        ctx: &mut VmContext,
        _offset: i32,
    ) -> BackEdgeOsrOutcome {
        // Record back-edge for profiling even without JIT.
        if let Some(frame) = ctx.current_frame() {
            let module = ctx.module_table.get(frame.module_id);
            if let Some(func) = module.function(frame.function_index) {
                func.record_back_edge();
                func.mark_hot();
            }
        }
        BackEdgeOsrOutcome::ContinueWithJump
    }
}
