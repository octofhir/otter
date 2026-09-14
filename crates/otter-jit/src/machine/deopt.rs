//! Allocator-driven lowering of Machine IR frame states.
//!
//! # Contents
//! - [`MachineFrameState`] — one exact chain of interpreter-register snapshots.
//! - [`MachineFrameSlot`] — allocated value or compile-time literal recipe.
//! - [`lower_deopt_table`] — conversion into the VM's one current [`DeoptTable`].
//!
//! # Invariants
//! - Value slots resolve only through late deopt operands retained by regalloc2.
//! - Integer, floating-point, and spill namespaces are unified deterministically.
//! - Caller/callee registers, this, closure and new.target use allocator locations.
//! - Every output frame is register-count wide and every deopt id is dense.
//! - Target emitters consume the same locations; no pre-allocation fallback exists.

use otter_vm::{
    Value,
    deopt::{
        DeoptFrame, DeoptFrameEntry, DeoptLocation, DeoptRepr, DeoptSlot, DeoptTable,
        DeoptVerifyError, DeoptVerifyLimits, FrameState,
    },
};

use super::{
    AllocatedLocation, AllocatedSequence, DeoptId, FrameLayoutError, InstructionSequence,
    MachineFrameLayout, MachineRepresentation, MachineValue,
};

/// One interpreter register at a Machine IR exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineFrameSlot {
    /// Value kept live through a late deopt operand.
    Value(MachineValue),
    /// Full tagged literal rematerialized only on the cold exit.
    TaggedLiteral(u64),
}

/// Exact outermost-first frame chain for one Machine IR deopt exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineFrameState {
    /// Dense exit identity attached to the owning instruction.
    pub id: DeoptId,
    /// VM-owned frame schema with allocator inputs instead of concrete recipes.
    pub frames: Box<[DeoptFrame<MachineFrameSlot>]>,
}

/// Failure to lower allocator locations into VM deopt metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineDeoptError {
    /// Frame states are not dense in deopt-id order.
    NonDenseId {
        /// Dense id required at this table position.
        expected: u32,
        /// Id supplied by the frame state.
        actual: u32,
    },
    /// A state references a missing Machine IR value.
    InvalidValue(MachineValue),
    /// No late allocator location exists for a state value at this exit.
    MissingLocation(DeoptId, MachineValue),
    /// A representation cannot reconstruct a JavaScript register value.
    UnsupportedRepresentation(MachineValue, MachineRepresentation),
    /// Register namespaces exceed the declared target budgets.
    RegisterOutOfRange,
    /// Frame-layout arithmetic failed.
    FrameLayout(FrameLayoutError),
    /// The lowered table violates the VM schema.
    Verification(DeoptVerifyError),
}

impl std::fmt::Display for MachineDeoptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Machine IR deopt lowering failed: {self:?}")
    }
}

impl std::error::Error for MachineDeoptError {}

/// Lower exact post-allocation locations into the VM's deopt table.
pub fn lower_deopt_table(
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
    layout: MachineFrameLayout,
    gpr_budget: u16,
    fp_budget: u16,
    states: &[MachineFrameState],
) -> Result<DeoptTable, MachineDeoptError> {
    let mut lowered = Vec::with_capacity(states.len());
    for (expected, state) in states.iter().enumerate() {
        let expected = expected as u32;
        if state.id.0 != expected {
            return Err(MachineDeoptError::NonDenseId {
                expected,
                actual: state.id.0,
            });
        }
        let frames = state
            .frames
            .iter()
            .map(|frame| {
                let lower =
                    |slot| lower_slot(sequence, allocation, layout, gpr_budget, state.id, slot);
                Ok(DeoptFrame {
                    function_id: frame.function_id,
                    byte_pc: frame.byte_pc,
                    entry: frame
                        .entry
                        .map(|entry| -> Result<_, MachineDeoptError> {
                            Ok(DeoptFrameEntry {
                                new_target: lower(entry.new_target)?,
                                return_register: entry.return_register,
                                this: lower(entry.this)?,
                                closure: lower(entry.closure)?,
                            })
                        })
                        .transpose()?,
                    slots: frame
                        .slots
                        .iter()
                        .copied()
                        .map(lower)
                        .collect::<Result<_, _>>()?,
                })
            })
            .collect::<Result<_, MachineDeoptError>>()?;
        lowered.push(FrameState { frames });
    }
    let table = DeoptTable::from_states(lowered);
    let max_stack_offset = if allocation.spill_slots() == 0 {
        0
    } else {
        i32::try_from(
            layout
                .spill_offset(allocation.spill_slots() - 1)
                .map_err(MachineDeoptError::FrameLayout)?,
        )
        .map_err(|_| MachineDeoptError::RegisterOutOfRange)?
    };
    table
        .verify(DeoptVerifyLimits {
            max_frame_slots: states
                .iter()
                .flat_map(|state| state.frames.iter().map(|frame| frame.slots.len()))
                .max()
                .unwrap_or(0),
            machine_register_count: gpr_budget
                .checked_add(fp_budget)
                .ok_or(MachineDeoptError::RegisterOutOfRange)?,
            min_stack_slot_offset: 0,
            max_stack_slot_offset: max_stack_offset,
        })
        .map_err(MachineDeoptError::Verification)?;
    Ok(table)
}

fn lower_slot(
    sequence: &InstructionSequence,
    allocation: &AllocatedSequence,
    layout: MachineFrameLayout,
    gpr_budget: u16,
    deopt: DeoptId,
    slot: MachineFrameSlot,
) -> Result<DeoptSlot, MachineDeoptError> {
    let MachineFrameSlot::Value(value) = slot else {
        let MachineFrameSlot::TaggedLiteral(bits) = slot else {
            unreachable!("MachineFrameSlot has two variants")
        };
        return Ok(DeoptSlot {
            location: DeoptLocation::Literal(bits),
            repr: DeoptRepr::Tagged,
        });
    };
    let representation = *sequence
        .representations()
        .get(value.0 as usize)
        .ok_or(MachineDeoptError::InvalidValue(value))?;
    let repr = match representation {
        MachineRepresentation::Tagged => DeoptRepr::Tagged,
        MachineRepresentation::Int32 => DeoptRepr::Int32,
        MachineRepresentation::Boolean => DeoptRepr::Boolean,
        MachineRepresentation::Uint32 => DeoptRepr::Uint32,
        MachineRepresentation::Float64 => DeoptRepr::Float64,
        MachineRepresentation::Cell
        | MachineRepresentation::Int64
        | MachineRepresentation::NativeStatus => {
            return Err(MachineDeoptError::UnsupportedRepresentation(
                value,
                representation,
            ));
        }
    };
    let location = allocation
        .metadata()
        .iter()
        .find(|metadata| metadata.deopt == Some(deopt) && metadata.value == value)
        .map(|metadata| metadata.location)
        .ok_or(MachineDeoptError::MissingLocation(deopt, value))?;
    let location = match location {
        AllocatedLocation::Register(register) if register.is_integer() => {
            let register = u16::from(register.encoding());
            if register >= gpr_budget {
                return Err(MachineDeoptError::RegisterOutOfRange);
            }
            DeoptLocation::Register(register)
        }
        AllocatedLocation::Register(register) if register.is_float() => DeoptLocation::Register(
            gpr_budget
                .checked_add(u16::from(register.encoding()))
                .ok_or(MachineDeoptError::RegisterOutOfRange)?,
        ),
        AllocatedLocation::Register(_) => return Err(MachineDeoptError::RegisterOutOfRange),
        AllocatedLocation::Stack(slot) => DeoptLocation::StackSlot(
            i32::try_from(
                layout
                    .spill_offset(slot)
                    .map_err(MachineDeoptError::FrameLayout)?,
            )
            .map_err(|_| MachineDeoptError::RegisterOutOfRange)?,
        ),
    };
    Ok(DeoptSlot { location, repr })
}

/// Canonical literal used for dead or uninitialized VM registers.
#[must_use]
pub fn undefined_slot() -> MachineFrameSlot {
    MachineFrameSlot::TaggedLiteral(Value::undefined().to_bits())
}

#[cfg(test)]
mod tests {
    use otter_vm::deopt::{DeoptExitId, DeoptLocation, DeoptRepr};

    use super::*;
    use crate::machine::{
        ControlFlow, InstructionSequence, MachineBlock, MachineBlockData, MachineInstruction,
        MachineInstructionId, MachineOpcode, MachineOperand, TargetSpec,
    };

    fn allocated_exit(
        target: &TargetSpec,
    ) -> (InstructionSequence, AllocatedSequence, MachineFrameLayout) {
        let integer = MachineValue(0);
        let float = MachineValue(1);
        let instructions = vec![
            MachineInstruction::plain(
                MachineOpcode::IntegerConstant(7),
                vec![MachineOperand::register_output(integer)],
            ),
            MachineInstruction::plain(
                MachineOpcode::FloatConstant(3.5_f64.to_bits()),
                vec![MachineOperand::register_output(float)],
            ),
            {
                let mut exit = MachineInstruction::plain(
                    MachineOpcode::Return,
                    vec![
                        MachineOperand::register_input(integer),
                        MachineOperand::deopt(integer),
                        MachineOperand::deopt(float),
                    ],
                );
                exit.deopt = Some(DeoptId(0));
                exit.control = ControlFlow::Return;
                exit
            },
        ];
        let sequence = InstructionSequence::new(
            target,
            MachineBlock(0),
            vec![MachineRepresentation::Int32, MachineRepresentation::Float64],
            Vec::new(),
            vec![MachineBlockData {
                first: MachineInstructionId(0),
                end: MachineInstructionId(3),
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                successor_arguments: Vec::new(),
            }],
            instructions,
        )
        .expect("valid deopt sequence");
        let allocation = sequence
            .allocate(target)
            .expect("deopt sequence allocation");
        let layout = target
            .frame_layout(&allocation, 0, 0)
            .expect("deopt frame layout");
        (sequence, allocation, layout)
    }

    #[test]
    fn lowers_caller_callee_and_entry_from_one_allocation() {
        let integer = MachineFrameSlot::Value(MachineValue(0));
        let float = MachineFrameSlot::Value(MachineValue(1));
        let frames = Box::new([
            DeoptFrame {
                function_id: 71,
                byte_pc: 24,
                entry: None,
                slots: Box::new([integer, float]),
            },
            DeoptFrame {
                function_id: 72,
                byte_pc: 8,
                entry: Some(DeoptFrameEntry {
                    new_target: float,
                    return_register: 1,
                    this: float,
                    closure: integer,
                }),
                slots: Box::new([float, integer]),
            },
        ]);
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            let (sequence, allocation, layout) = allocated_exit(&target);
            let (gpr_budget, fp_budget) = target.deopt_register_budgets();
            let table = lower_deopt_table(
                &sequence,
                &allocation,
                layout,
                gpr_budget,
                fp_budget,
                &[MachineFrameState {
                    id: DeoptId(0),
                    frames: frames.clone(),
                }],
            )
            .unwrap();
            let state = table.lookup(DeoptExitId(0)).unwrap();
            assert_eq!(state.frames.len(), 2);
            let caller = state.outermost();
            let callee = state.innermost();
            assert_eq!(callee.function_id, 72);
            assert_eq!(callee.byte_pc, 8);
            assert_eq!(callee.slots[0], caller.slots[1]);
            assert_eq!(callee.slots[1], caller.slots[0]);
            let entry = callee.entry.unwrap();
            assert_eq!(entry.return_register, 1);
            assert_eq!(entry.this, caller.slots[1]);
            assert_eq!(entry.closure, caller.slots[0]);
            assert_eq!(entry.new_target, caller.slots[1]);
        }
    }

    #[test]
    fn lowers_exact_allocator_locations_and_literal_slots() {
        let target = TargetSpec::aarch64();
        let (sequence, allocation, layout) = allocated_exit(&target);
        let (gpr_budget, fp_budget) = target.deopt_register_budgets();
        let table = lower_deopt_table(
            &sequence,
            &allocation,
            layout,
            gpr_budget,
            fp_budget,
            &[MachineFrameState {
                id: DeoptId(0),
                frames: Box::new([DeoptFrame {
                    function_id: 71,
                    byte_pc: 24,
                    entry: None,
                    slots: vec![
                        MachineFrameSlot::Value(MachineValue(0)),
                        MachineFrameSlot::Value(MachineValue(1)),
                        undefined_slot(),
                    ]
                    .into_boxed_slice(),
                }]),
            }],
        )
        .expect("allocator-driven deopt table");

        let frame = table
            .lookup(DeoptExitId(0))
            .expect("dense exit")
            .outermost();
        assert_eq!(frame.function_id, 71);
        assert_eq!(frame.byte_pc, 24);
        assert_eq!(frame.slots[0].repr, DeoptRepr::Int32);
        assert_eq!(frame.slots[1].repr, DeoptRepr::Float64);
        assert_eq!(frame.slots[2].repr, DeoptRepr::Tagged);
        assert!(matches!(
            frame.slots[0].location,
            DeoptLocation::Register(_)
        ));
        assert!(matches!(
            frame.slots[1].location,
            DeoptLocation::Register(_)
        ));
        assert_eq!(
            frame.slots[2].location,
            DeoptLocation::Literal(Value::undefined().to_bits())
        );
        assert_ne!(frame.slots[0].location, frame.slots[1].location);
    }
}
