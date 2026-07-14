//! Backend-independent lowering of abstract frame states to concrete deopt metadata.
//!
//! # Contents
//! - [`DeoptLowering`] — a verified concrete exact-byte-PC deopt table.
//! - [`DeoptLowering::build`] — location and representation lowering.
//! - [`DeoptLowering::verify`] — pure reconstruction-metadata verification.
//! - [`DeoptLoweringError`] — precise construction and verification failures.
//!
//! # Invariants
//! - Every abstract canonical-PC state maps to exactly one concrete byte-PC state.
//! - Concrete slots are register-count wide and remain in interpreter-register order.
//! - Register allocation locations and selected representations are recomputed per slot.
//! - GPR deopt register IDs and spill offsets retain their existing numbering;
//!   FP registers and spills follow their GPR namespace to prevent collisions.
//! - A missing abstract value maps to a register-specific undefined constant with a
//!   tagged representation, keeping multiple dead registers location-distinct.
//!
//! # See also
//! - [`super::frame_state`] for abstract interpreter-register state.
//! - [`super::regalloc`] for backend-independent value locations.
//! - [`super::repr`] for selected SSA value representations.
//! - [`otter_vm::deopt`] for the concrete VM deoptimization schema.

use std::collections::BTreeSet;

use otter_vm::{
    JitCompileSnapshot,
    deopt::{
        DeoptLocation, DeoptRepr, DeoptSlot, DeoptTable, DeoptVerifyError, DeoptVerifyLimits,
        FrameState,
    },
};

use super::{
    frame_state::FrameStateTable,
    regalloc::{Allocation, Location, RegClass},
    repr::{ReprError, ReprMap, Representation},
    ssa::{SsaFunction, ValueId},
};

const STACK_SLOT_BYTES: u32 = std::mem::size_of::<u64>() as u32;

/// Concrete, verified deoptimization metadata for one immutable compile snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeoptLowering {
    table: DeoptTable,
}

/// Failure to lower or verify concrete deoptimization metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeoptLoweringError {
    /// Representation selection does not match the snapshot and SSA graph.
    InvalidRepresentations(ReprError),
    /// The allocation does not contain exactly one location per SSA value.
    AllocationLocationCountMismatch {
        /// Number of SSA values requiring locations.
        expected: usize,
        /// Number of stored allocation locations.
        actual: usize,
    },
    /// A value allocation belongs to a different class than its representation.
    AllocationClassMismatch {
        /// Value with the invalid allocation.
        value: ValueId,
        /// Representation-derived register class.
        expected: RegClass,
        /// Class stored in the allocation.
        actual: RegClass,
    },
    /// Abstract frame-state coverage differs from the snapshot instruction count.
    AbstractStateCountMismatch {
        /// Number of snapshot instructions requiring states.
        expected: usize,
        /// Number of abstract states supplied.
        actual: usize,
    },
    /// SSA instruction coverage differs from the snapshot instruction count.
    SsaInstructionCountMismatch {
        /// Number of snapshot instructions.
        expected: usize,
        /// Number of SSA instructions.
        actual: usize,
    },
    /// An abstract state is not stored at its canonical dense instruction PC.
    AbstractStateOrderMismatch {
        /// Dense canonical PC required at this position.
        expected: u32,
        /// Canonical PC stored in the abstract state.
        actual: u32,
    },
    /// Snapshot instruction metadata disagrees with its canonical dense position.
    InstructionPcMismatch {
        /// Dense canonical PC required at this position.
        expected: u32,
        /// Canonical PC reported by the snapshot instruction.
        actual: u32,
    },
    /// An abstract state is not register-count wide.
    AbstractSlotCountMismatch {
        /// Canonical instruction PC owning the state.
        pc: u32,
        /// Function interpreter-register count.
        expected: usize,
        /// Number of abstract register slots.
        actual: usize,
    },
    /// An abstract register slot references no dense SSA value.
    ValueOutOfRange {
        /// Canonical instruction PC owning the state.
        pc: u32,
        /// Interpreter register containing the invalid value.
        register: u16,
        /// Invalid SSA value identity.
        value: ValueId,
        /// Number of valid SSA values.
        value_count: usize,
    },
    /// A value allocation names a machine register outside the declared register file.
    MachineRegisterOutOfRange {
        /// Value with the invalid allocation.
        value: ValueId,
        /// Register class containing the invalid index.
        class: RegClass,
        /// Invalid backend register index.
        register: u8,
        /// Number of registers available to values.
        register_count: u8,
    },
    /// A value allocation names a spill index outside the declared spill area.
    SpillSlotOutOfRange {
        /// Value with the invalid allocation.
        value: ValueId,
        /// Spill namespace containing the invalid slot.
        class: RegClass,
        /// Invalid spill-slot index.
        slot: u32,
        /// Number of allocated spill slots.
        spill_slot_count: u32,
    },
    /// A spill-slot index cannot be represented as an aligned signed byte offset.
    SpillSlotOffsetOverflow {
        /// Spill-slot index that overflowed the concrete schema.
        slot: u32,
    },
    /// Two abstract points resolve to the same exact byte PC.
    DuplicateBytePc {
        /// Duplicated byte PC.
        byte_pc: u32,
    },
    /// The number of concrete states differs from abstract state coverage.
    ConcreteStateCountMismatch {
        /// Number of abstract deopt points.
        expected: usize,
        /// Number of concrete frame states.
        actual: usize,
    },
    /// No concrete frame state exists at an abstract point's exact byte PC.
    MissingConcreteState {
        /// Abstract canonical instruction PC.
        pc: u32,
        /// Required exact byte PC.
        byte_pc: u32,
    },
    /// A concrete frame state is not register-count wide.
    ConcreteSlotCountMismatch {
        /// Exact byte PC owning the state.
        byte_pc: u32,
        /// Function interpreter-register count.
        expected: usize,
        /// Number of concrete slots.
        actual: usize,
    },
    /// A concrete slot's location differs from the allocation-derived location.
    SlotLocationMismatch {
        /// Exact byte PC owning the slot.
        byte_pc: u32,
        /// Interpreter register index.
        register: u16,
        /// Recomputed concrete location.
        expected: DeoptLocation,
        /// Stored concrete location.
        actual: DeoptLocation,
    },
    /// A concrete slot's representation differs from representation selection.
    SlotRepresentationMismatch {
        /// Exact byte PC owning the slot.
        byte_pc: u32,
        /// Interpreter register index.
        register: u16,
        /// Recomputed concrete representation.
        expected: DeoptRepr,
        /// Stored concrete representation.
        actual: DeoptRepr,
    },
    /// The concrete VM deopt table violates its own schema invariants.
    InvalidDeoptTable(DeoptVerifyError),
    /// Repeating lowering over identical immutable inputs changed the output.
    NonDeterministic,
}

impl std::fmt::Display for DeoptLoweringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid lowered deopt metadata: {self:?}")
    }
}

impl std::error::Error for DeoptLoweringError {}

impl DeoptLowering {
    /// Lower abstract interpreter-register state to the concrete VM deopt schema.
    pub fn build(
        view: &JitCompileSnapshot,
        ssa: &SsaFunction,
        frame_states: &FrameStateTable,
        allocation: &Allocation,
        reprs: &ReprMap,
    ) -> Result<Self, DeoptLoweringError> {
        let table = lower_table(view, ssa, frame_states, allocation, reprs)?;
        let lowering = Self { table };
        lowering.verify(view, ssa, frame_states, allocation, reprs)?;
        Ok(lowering)
    }

    /// Borrow the verified concrete exact-byte-PC deopt table.
    #[must_use]
    pub fn table(&self) -> &DeoptTable {
        &self.table
    }

    /// Purely verify coverage, exact PCs, slot mappings, schema validity, and determinism.
    pub fn verify(
        &self,
        view: &JitCompileSnapshot,
        ssa: &SsaFunction,
        frame_states: &FrameStateTable,
        allocation: &Allocation,
        reprs: &ReprMap,
    ) -> Result<(), DeoptLoweringError> {
        validate_inputs(view, ssa, frame_states, allocation, reprs)?;
        self.table
            .verify(verify_limits(ssa, allocation)?)
            .map_err(DeoptLoweringError::InvalidDeoptTable)?;

        if self.table.len() != frame_states.states().len() {
            return Err(DeoptLoweringError::ConcreteStateCountMismatch {
                expected: frame_states.states().len(),
                actual: self.table.len(),
            });
        }

        for state in frame_states.states() {
            let byte_pc = byte_pc(view, state.pc)?;
            let concrete =
                self.table
                    .lookup(byte_pc)
                    .ok_or(DeoptLoweringError::MissingConcreteState {
                        pc: state.pc,
                        byte_pc,
                    })?;
            let expected_width = usize::from(ssa.register_count);
            if concrete.slots.len() != expected_width {
                return Err(DeoptLoweringError::ConcreteSlotCountMismatch {
                    byte_pc,
                    expected: expected_width,
                    actual: concrete.slots.len(),
                });
            }
            for (register, &value) in state.registers.iter().enumerate() {
                let register = register as u16;
                let expected = lower_slot(register, value, ssa, allocation, reprs, state.pc)?;
                let actual = concrete.slots[usize::from(register)];
                if actual.location != expected.location {
                    return Err(DeoptLoweringError::SlotLocationMismatch {
                        byte_pc,
                        register,
                        expected: expected.location,
                        actual: actual.location,
                    });
                }
                if actual.repr != expected.repr {
                    return Err(DeoptLoweringError::SlotRepresentationMismatch {
                        byte_pc,
                        register,
                        expected: expected.repr,
                        actual: actual.repr,
                    });
                }
            }
        }

        let first = lower_table(view, ssa, frame_states, allocation, reprs)?;
        let second = lower_table(view, ssa, frame_states, allocation, reprs)?;
        if first != second || self.table != first {
            return Err(DeoptLoweringError::NonDeterministic);
        }
        Ok(())
    }
}

fn validate_inputs(
    view: &JitCompileSnapshot,
    ssa: &SsaFunction,
    frame_states: &FrameStateTable,
    allocation: &Allocation,
    reprs: &ReprMap,
) -> Result<(), DeoptLoweringError> {
    reprs
        .verify(view, ssa)
        .map_err(DeoptLoweringError::InvalidRepresentations)?;
    if allocation.locations.len() != ssa.values.len() {
        return Err(DeoptLoweringError::AllocationLocationCountMismatch {
            expected: ssa.values.len(),
            actual: allocation.locations.len(),
        });
    }
    if frame_states.states().len() != view.instructions.len() {
        return Err(DeoptLoweringError::AbstractStateCountMismatch {
            expected: view.instructions.len(),
            actual: frame_states.states().len(),
        });
    }
    let ssa_instruction_count = ssa.blocks.iter().map(|block| block.instrs.len()).sum();
    if ssa_instruction_count != view.instructions.len() {
        return Err(DeoptLoweringError::SsaInstructionCountMismatch {
            expected: view.instructions.len(),
            actual: ssa_instruction_count,
        });
    }

    let expected_width = usize::from(ssa.register_count);
    let mut byte_pcs = BTreeSet::new();
    for (index, state) in frame_states.states().iter().enumerate() {
        let expected_pc =
            u32::try_from(index).map_err(|_| DeoptLoweringError::AbstractStateOrderMismatch {
                expected: u32::MAX,
                actual: state.pc,
            })?;
        if state.pc != expected_pc {
            return Err(DeoptLoweringError::AbstractStateOrderMismatch {
                expected: expected_pc,
                actual: state.pc,
            });
        }
        let instruction = &view.instructions[index];
        let actual_pc = instruction.instruction_pc(&view.code_block);
        if actual_pc != expected_pc {
            return Err(DeoptLoweringError::InstructionPcMismatch {
                expected: expected_pc,
                actual: actual_pc,
            });
        }
        if state.registers.len() != expected_width {
            return Err(DeoptLoweringError::AbstractSlotCountMismatch {
                pc: state.pc,
                expected: expected_width,
                actual: state.registers.len(),
            });
        }
        if !byte_pcs.insert(instruction.byte_pc) {
            return Err(DeoptLoweringError::DuplicateBytePc {
                byte_pc: instruction.byte_pc,
            });
        }
        for (register, &value) in state.registers.iter().enumerate() {
            lower_slot(register as u16, value, ssa, allocation, reprs, state.pc)?;
        }
    }
    Ok(())
}

fn lower_table(
    view: &JitCompileSnapshot,
    ssa: &SsaFunction,
    frame_states: &FrameStateTable,
    allocation: &Allocation,
    reprs: &ReprMap,
) -> Result<DeoptTable, DeoptLoweringError> {
    validate_inputs(view, ssa, frame_states, allocation, reprs)?;
    let mut states = Vec::with_capacity(frame_states.states().len());
    for state in frame_states.states() {
        let slots = state
            .registers
            .iter()
            .enumerate()
            .map(|(register, &value)| {
                lower_slot(register as u16, value, ssa, allocation, reprs, state.pc)
            })
            .collect::<Result<Vec<_>, _>>()?;
        states.push(FrameState {
            byte_pc: byte_pc(view, state.pc)?,
            slots: slots.into_boxed_slice(),
        });
    }
    Ok(DeoptTable::from_states(states))
}

fn lower_slot(
    register: u16,
    value: Option<ValueId>,
    ssa: &SsaFunction,
    allocation: &Allocation,
    reprs: &ReprMap,
    pc: u32,
) -> Result<DeoptSlot, DeoptLoweringError> {
    let Some(value) = value else {
        // Unreachable/dead abstract registers cannot be read before a definition
        // on a bytecode-valid path. Each register reserves its own constant-pool
        // entry containing `undefined`, preserving concrete-location uniqueness.
        return Ok(DeoptSlot {
            location: DeoptLocation::Constant(u32::from(register)),
            repr: DeoptRepr::Tagged,
        });
    };
    if value.0 as usize >= ssa.values.len() {
        return Err(DeoptLoweringError::ValueOutOfRange {
            pc,
            register,
            value,
            value_count: ssa.values.len(),
        });
    }
    let representation = reprs.representation(value);
    let expected_class = RegClass::from_representation(representation);
    let allocated_location = allocation.location(value);
    if allocated_location.class() != expected_class {
        return Err(DeoptLoweringError::AllocationClassMismatch {
            value,
            expected: expected_class,
            actual: allocated_location.class(),
        });
    }
    let location = match allocated_location {
        Location::Register(class, allocated) => {
            let register_count = match class {
                RegClass::Gpr => allocation.register_budget.gpr,
                RegClass::Fp => allocation.register_budget.fp,
            };
            if allocated >= register_count {
                return Err(DeoptLoweringError::MachineRegisterOutOfRange {
                    value,
                    class,
                    register: allocated,
                    register_count,
                });
            }
            let unified = match class {
                RegClass::Gpr => u16::from(allocated),
                RegClass::Fp => u16::from(allocation.register_budget.gpr) + u16::from(allocated),
            };
            DeoptLocation::Register(unified)
        }
        Location::Spill(class, slot) => {
            let spill_slot_count = match class {
                RegClass::Gpr => allocation.spill_slot_counts.gpr,
                RegClass::Fp => allocation.spill_slot_counts.fp,
            };
            if slot >= spill_slot_count {
                return Err(DeoptLoweringError::SpillSlotOutOfRange {
                    value,
                    class,
                    slot,
                    spill_slot_count,
                });
            }
            let unified = match class {
                RegClass::Gpr => slot,
                RegClass::Fp => allocation
                    .spill_slot_counts
                    .gpr
                    .checked_add(slot)
                    .ok_or(DeoptLoweringError::SpillSlotOffsetOverflow { slot })?,
            };
            DeoptLocation::StackSlot(spill_offset(unified)?)
        }
    };
    let repr = match representation {
        Representation::Int32 => DeoptRepr::Int32,
        Representation::Float64 => DeoptRepr::Float64,
        Representation::Tagged => DeoptRepr::Tagged,
    };
    Ok(DeoptSlot { location, repr })
}

fn spill_offset(slot: u32) -> Result<i32, DeoptLoweringError> {
    let bytes = slot
        .checked_mul(STACK_SLOT_BYTES)
        .and_then(|offset| i32::try_from(offset).ok())
        .ok_or(DeoptLoweringError::SpillSlotOffsetOverflow { slot })?;
    Ok(bytes)
}

fn byte_pc(view: &JitCompileSnapshot, pc: u32) -> Result<u32, DeoptLoweringError> {
    let index = usize::try_from(pc).map_err(|_| DeoptLoweringError::InstructionPcMismatch {
        expected: u32::MAX,
        actual: pc,
    })?;
    let instruction =
        view.instructions
            .get(index)
            .ok_or(DeoptLoweringError::InstructionPcMismatch {
                expected: view.instructions.len().try_into().unwrap_or(u32::MAX),
                actual: pc,
            })?;
    Ok(instruction.byte_pc)
}

fn verify_limits(
    ssa: &SsaFunction,
    allocation: &Allocation,
) -> Result<DeoptVerifyLimits, DeoptLoweringError> {
    let spill_slot_count = allocation
        .spill_slot_counts
        .gpr
        .checked_add(allocation.spill_slot_counts.fp)
        .ok_or(DeoptLoweringError::SpillSlotOffsetOverflow { slot: u32::MAX })?;
    let max_stack_slot_offset = if spill_slot_count == 0 {
        0
    } else {
        spill_offset(spill_slot_count - 1)?
    };
    Ok(DeoptVerifyLimits {
        max_frame_slots: usize::from(ssa.register_count),
        machine_register_count: u16::from(allocation.register_budget.gpr)
            + u16::from(allocation.register_budget.fp),
        min_stack_slot_offset: 0,
        max_stack_slot_offset,
        constant_count: u32::from(ssa.register_count),
    })
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{Op, Operand};
    use otter_vm::{
        deopt::{DeoptVerifyError, DeoptVerifyLimits},
        jit::JitTestInstruction,
        jit_feedback::{ARITH_FLOAT64, ARITH_INT32, ArithFeedback},
    };

    use super::*;
    use crate::ir::{
        cfg::ControlFlowGraph, dom::DominatorTree, liveness::Liveness, regalloc::RegisterBudget,
        ssa::ValueDef,
    };

    struct Pipeline {
        view: JitCompileSnapshot,
        ssa: SsaFunction,
        frame_states: FrameStateTable,
        allocation: Allocation,
        reprs: ReprMap,
    }

    fn pipeline(
        param_count: u16,
        register_count: u16,
        register_budget: RegisterBudget,
        instructions: Vec<(Op, Vec<Operand>)>,
        feedback: &[(u32, u8)],
    ) -> Pipeline {
        let instructions = instructions
            .into_iter()
            .enumerate()
            .map(|(pc, (op, operands))| {
                JitTestInstruction::new(op, pc as u32, pc as u32 * 8 + 3, operands)
            })
            .collect();
        let mut view =
            JitCompileSnapshot::without_feedback(7, param_count, register_count, instructions);
        for &(pc, bits) in feedback {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(bits));
        }
        let cfg = ControlFlowGraph::build(&view).expect("CFG builds");
        cfg.verify().expect("CFG verifies");
        let ssa = SsaFunction::build(&view, &cfg).expect("SSA builds");
        let dom = DominatorTree::compute(&cfg);
        ssa.verify(&cfg, &dom).expect("SSA verifies");
        let liveness = Liveness::compute(&ssa, &cfg);
        liveness
            .verify(&ssa, &cfg, &dom)
            .expect("liveness verifies");
        let reprs = ReprMap::compute(&view, &ssa);
        reprs.verify(&view, &ssa).expect("representations verify");
        let allocation = Allocation::compute(&ssa, &cfg, &liveness, &reprs, register_budget)
            .expect("allocation computes");
        allocation
            .verify(&ssa, &cfg, &liveness, &reprs)
            .expect("allocation verifies");
        let frame_states = FrameStateTable::build(&ssa, &cfg).expect("frame states build");
        frame_states
            .verify(&ssa, &cfg, &dom)
            .expect("frame states verify");
        Pipeline {
            view,
            ssa,
            frame_states,
            allocation,
            reprs,
        }
    }

    const fn budget(gpr: u8, fp: u8) -> RegisterBudget {
        RegisterBudget { gpr, fp }
    }

    fn lower(pipeline: &Pipeline) -> DeoptLowering {
        DeoptLowering::build(
            &pipeline.view,
            &pipeline.ssa,
            &pipeline.frame_states,
            &pipeline.allocation,
            &pipeline.reprs,
        )
        .expect("deopt lowering builds")
    }

    fn op_value_at(ssa: &SsaFunction, pc: u32) -> ValueId {
        ssa.values
            .iter()
            .find_map(|value| match value.def {
                ValueDef::Op { pc: owner, .. } if owner == pc => Some(value.id),
                _ => None,
            })
            .expect("instruction has an SSA result")
    }

    fn concrete_at<'a>(
        lowering: &'a DeoptLowering,
        pipeline: &Pipeline,
        pc: u32,
    ) -> &'a FrameState {
        lowering
            .table()
            .lookup(pipeline.view.instructions[pc as usize].byte_pc)
            .expect("exact byte PC is lowered")
    }

    #[test]
    fn int32_at_allocated_location() {
        let pipeline = pipeline(
            0,
            1,
            budget(8, 8),
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(9)]),
                (Op::Nop, vec![]),
                (Op::ReturnUndefined, vec![]),
            ],
            &[],
        );
        let lowering = lower(&pipeline);
        let value = op_value_at(&pipeline.ssa, 0);
        let slot = concrete_at(&lowering, &pipeline, 1).slots[0];

        assert_eq!(slot.repr, DeoptRepr::Int32);
        assert_eq!(
            slot,
            lower_slot(
                0,
                Some(value),
                &pipeline.ssa,
                &pipeline.allocation,
                &pipeline.reprs,
                1
            )
            .unwrap()
        );
        assert_eq!(
            lowering.verify(
                &pipeline.view,
                &pipeline.ssa,
                &pipeline.frame_states,
                &pipeline.allocation,
                &pipeline.reprs
            ),
            Ok(())
        );
    }

    #[test]
    fn float64_fp_register_uses_disjoint_deopt_register_id() {
        let pipeline = pipeline(
            0,
            1,
            budget(3, 1),
            vec![
                (
                    Op::LoadNumber,
                    vec![Operand::Register(0), Operand::ConstIndex(0)],
                ),
                (Op::Nop, vec![]),
                (Op::ReturnUndefined, vec![]),
            ],
            &[(0, ARITH_FLOAT64)],
        );
        let value = op_value_at(&pipeline.ssa, 0);
        assert_eq!(
            pipeline.allocation.location(value),
            Location::Register(RegClass::Fp, 0)
        );

        let lowering = lower(&pipeline);
        let slot = concrete_at(&lowering, &pipeline, 1).slots[0];
        assert_eq!(slot.repr, DeoptRepr::Float64);
        assert_eq!(slot.location, DeoptLocation::Register(3));
        assert_eq!(
            lowering.verify(
                &pipeline.view,
                &pipeline.ssa,
                &pipeline.frame_states,
                &pipeline.allocation,
                &pipeline.reprs,
            ),
            Ok(())
        );
    }

    #[test]
    fn spilled_value_maps_to_stack_slot() {
        let pipeline = pipeline(
            2,
            3,
            budget(1, 1),
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnUndefined, vec![]),
            ],
            &[(0, ARITH_INT32)],
        );
        let lowering = lower(&pipeline);
        let (value, spill) = pipeline
            .allocation
            .locations
            .iter()
            .enumerate()
            .find_map(|(value, location)| match location {
                Location::Spill(RegClass::Gpr, slot) => Some((ValueId(value as u32), *slot)),
                Location::Register(_, _) | Location::Spill(RegClass::Fp, _) => None,
            })
            .expect("one-register allocation spills");
        let (pc, register) = pipeline
            .frame_states
            .states()
            .iter()
            .find_map(|state| {
                state
                    .registers
                    .iter()
                    .position(|candidate| *candidate == Some(value))
                    .map(|register| (state.pc, register))
            })
            .expect("spilled value appears in a frame state");
        let slot = concrete_at(&lowering, &pipeline, pc).slots[register];

        assert_eq!(
            slot.location,
            DeoptLocation::StackSlot(spill_offset(spill).unwrap())
        );
        assert_eq!(
            lowering.verify(
                &pipeline.view,
                &pipeline.ssa,
                &pipeline.frame_states,
                &pipeline.allocation,
                &pipeline.reprs
            ),
            Ok(())
        );
    }

    #[test]
    fn tagged_value_keeps_tagged_representation() {
        let pipeline = pipeline(
            0,
            1,
            budget(4, 4),
            vec![
                (Op::LoadUndefined, vec![Operand::Register(0)]),
                (Op::Nop, vec![]),
                (Op::ReturnUndefined, vec![]),
            ],
            &[],
        );
        let lowering = lower(&pipeline);
        let slot = concrete_at(&lowering, &pipeline, 1).slots[0];

        assert_eq!(slot.repr, DeoptRepr::Tagged);
        assert!(matches!(slot.location, DeoptLocation::Register(_)));
        assert_eq!(
            lowering.verify(
                &pipeline.view,
                &pipeline.ssa,
                &pipeline.frame_states,
                &pipeline.allocation,
                &pipeline.reprs
            ),
            Ok(())
        );
    }

    #[test]
    fn uninitialized_register_uses_undefined_constant() {
        let pipeline = pipeline(
            0,
            1,
            budget(4, 4),
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
                (Op::Nop, vec![]),
                (Op::ReturnUndefined, vec![]),
            ],
            &[],
        );
        let lowering = lower(&pipeline);
        let slot = lower_slot(
            0,
            None,
            &pipeline.ssa,
            &pipeline.allocation,
            &pipeline.reprs,
            1,
        )
        .expect("a dead register lowers without reading allocation state");

        assert_eq!(slot.location, DeoptLocation::Constant(0));
        assert_eq!(slot.repr, DeoptRepr::Tagged);
        assert_eq!(
            DeoptRepr::Tagged.reconstitute(otter_vm::Value::undefined().to_bits()),
            otter_vm::Value::undefined()
        );
        assert_eq!(
            lowering.verify(
                &pipeline.view,
                &pipeline.ssa,
                &pipeline.frame_states,
                &pipeline.allocation,
                &pipeline.reprs
            ),
            Ok(())
        );
    }

    #[test]
    fn per_pc_reaching_value_changes_location_or_representation() {
        let pipeline = pipeline(
            0,
            1,
            budget(4, 4),
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
                (Op::Nop, vec![]),
                (Op::LoadUndefined, vec![Operand::Register(0)]),
                (Op::ReturnUndefined, vec![]),
            ],
            &[],
        );
        let first = op_value_at(&pipeline.ssa, 0);
        let second = op_value_at(&pipeline.ssa, 2);
        assert_ne!(first, second);
        let lowering = lower(&pipeline);
        let first_slot = concrete_at(&lowering, &pipeline, 1).slots[0];
        let second_slot = concrete_at(&lowering, &pipeline, 3).slots[0];

        assert_eq!(
            first_slot,
            lower_slot(
                0,
                Some(first),
                &pipeline.ssa,
                &pipeline.allocation,
                &pipeline.reprs,
                1
            )
            .unwrap()
        );
        assert_eq!(
            second_slot,
            lower_slot(
                0,
                Some(second),
                &pipeline.ssa,
                &pipeline.allocation,
                &pipeline.reprs,
                3
            )
            .unwrap()
        );
        assert_eq!(first_slot.repr, DeoptRepr::Int32);
        assert_eq!(second_slot.repr, DeoptRepr::Tagged);
        assert_eq!(
            lowering.verify(
                &pipeline.view,
                &pipeline.ssa,
                &pipeline.frame_states,
                &pipeline.allocation,
                &pipeline.reprs
            ),
            Ok(())
        );
    }

    #[test]
    fn verifier_rejects_corrupted_slot_and_out_of_order_table() {
        let pipeline = pipeline(
            0,
            1,
            budget(4, 4),
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
                (Op::Nop, vec![]),
                (Op::ReturnUndefined, vec![]),
            ],
            &[],
        );
        let lowering = lower(&pipeline);
        let byte_pc = pipeline.view.instructions[1].byte_pc;
        let mut states: Vec<_> = pipeline
            .frame_states
            .states()
            .iter()
            .map(|state| concrete_at(&lowering, &pipeline, state.pc).clone())
            .collect();
        states[1].slots[0].repr = DeoptRepr::Tagged;
        let corrupted = DeoptLowering {
            table: DeoptTable::from_states(states),
        };
        assert_eq!(
            corrupted.verify(
                &pipeline.view,
                &pipeline.ssa,
                &pipeline.frame_states,
                &pipeline.allocation,
                &pipeline.reprs
            ),
            Err(DeoptLoweringError::SlotRepresentationMismatch {
                byte_pc,
                register: 0,
                expected: DeoptRepr::Int32,
                actual: DeoptRepr::Tagged,
            })
        );

        let out_of_order = DeoptTable::from_unverified_states(vec![
            FrameState {
                byte_pc: 20,
                slots: Box::new([]),
            },
            FrameState {
                byte_pc: 8,
                slots: Box::new([]),
            },
        ]);
        assert_eq!(
            out_of_order.verify(DeoptVerifyLimits {
                max_frame_slots: 0,
                machine_register_count: 0,
                min_stack_slot_offset: 0,
                max_stack_slot_offset: 0,
                constant_count: 0,
            }),
            Err(DeoptVerifyError::EntriesNotSorted {
                previous: 20,
                current: 8
            })
        );
    }
}
