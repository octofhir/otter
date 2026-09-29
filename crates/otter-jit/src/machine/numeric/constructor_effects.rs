//! Explicit constructor field guards, store, publication, and barriers.
//!
//! # Contents
//! - One `PropertyStoreDispatch` case owning the receiver, prototype,
//!   extensibility, length and capacity proofs plus the committed store and
//!   shape publication.
//! - A single pre-effect condition exit on the dispatch's hit bit.
//! - Value and child-shape GC barriers.
//!
//! # Invariants
//! - Every shape, prototype, extensibility, length, and capacity proof precedes
//!   the first store; a dispatch miss performs no effect.
//! - No exit is legal after the condition has been accepted.
//! - Field data and structural publication commit inside the one dispatch
//!   effect, after every guard.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn select(
    target_spec: &TargetSpec,
    hir: &NumericFunction,
    object: MachineValue,
    value: MachineValue,
    transition: &otter_vm::jit::JitConstructorFieldTransition,
    state_index: usize,
    exits: Box<[MachineExit]>,
    machine_values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<(), super::super::VerificationError> {
    let byte_pc = hir.frame_states[state_index]
        .frames
        .last()
        .map(|frame| frame.byte_pc)
        .ok_or(super::super::VerificationError::InvalidValue(object))?;
    // One store dispatch owns every shape, prototype, extensibility, length
    // and capacity proof and commits the store and publication together. A
    // miss performs no effect, so the following condition exits exactly at
    // the pre-operation state.
    let owner = push_value(representations, MachineRepresentation::Int64);
    let child = push_value(representations, MachineRepresentation::Tagged);
    let stored = push_value(representations, MachineRepresentation::Boolean);
    let mut store = MachineInstruction::plain(
        MachineOpcode::PropertyStoreDispatch {
            byte_pc,
            cases: Box::new([super::super::PropertyStoreCase {
                shape: transition.from_shape,
                value_byte: u32::from(transition.slot) * 8,
                transition: Some(super::super::PropertyStoreTransition {
                    prototype_shapes: transition.prototype_shapes.clone().into_boxed_slice(),
                    child_shape: transition.to_shape,
                }),
            }]),
        },
        vec![
            MachineOperand::location_input(object),
            MachineOperand::location_input(value),
            MachineOperand::register_output(owner),
            MachineOperand::register_output(child),
            MachineOperand::register_output(stored),
        ],
    );
    store.clobbers = property_store_clobbers(target_spec, false);
    instructions.push(store);

    let mut require = MachineInstruction::plain(
        MachineOpcode::GuardCondition,
        vec![MachineOperand::register_input(stored)],
    );
    require.clobbers = target_spec
        .clobbers(TargetClobberSet::StatusScratch)
        .to_vec();
    attach_frame_state(hir, machine_values, state_index, exits, &mut require);
    instructions.push(require);

    push_barrier(
        target_spec,
        byte_pc,
        owner,
        value,
        stored,
        state_index,
        instructions,
    );
    push_barrier(
        target_spec,
        byte_pc,
        owner,
        child,
        stored,
        state_index,
        instructions,
    );
    Ok(())
}

fn push_barrier(
    target_spec: &TargetSpec,
    byte_pc: u32,
    owner: MachineValue,
    value: MachineValue,
    condition: MachineValue,
    state_index: usize,
    instructions: &mut Vec<MachineInstruction>,
) {
    let mut barrier = MachineInstruction::plain(
        MachineOpcode::CacheIrWriteBarrier {
            byte_pc,
            value_is_non_cell: false,
        },
        vec![
            MachineOperand::location_input(owner),
            MachineOperand::location_input(value),
            MachineOperand::register_input(condition),
        ],
    );
    barrier.clobbers = property_store_clobbers(target_spec, false);
    barrier.frame_state = Some(state_index as u32);
    instructions.push(barrier);
}
