//! Explicit constructor field guards, store, publication, and barriers.
//!
//! # Contents
//! - Source-owned receiver/prototype proof selection.
//! - A single pre-effect condition exit.
//! - No-fail field store, shape publication, and GC barriers.
//!
//! # Invariants
//! - Every shape, prototype, extensibility, length, and capacity proof precedes
//!   the first store.
//! - No exit is legal after the condition has been accepted.
//! - Field data and structural publication are distinct Machine effects.

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
    let active = push_value(representations, MachineRepresentation::Boolean);
    instructions.push(MachineInstruction::plain(
        MachineOpcode::BooleanConstant(true),
        vec![MachineOperand::register_output(active)],
    ));
    let mut condition = active;
    let mut current = object;

    let next = push_value(representations, MachineRepresentation::Boolean);
    push_guard(
        target_spec,
        MachineOpcode::CacheIrGuardShape {
            byte_pc,
            shape: transition.from_shape,
        },
        vec![
            MachineOperand::location_input(current),
            MachineOperand::register_input(condition),
            MachineOperand::register_output(next),
        ],
        state_index,
        instructions,
    );
    condition = next;

    let next = push_value(representations, MachineRepresentation::Boolean);
    push_guard(
        target_spec,
        MachineOpcode::CacheIrGuardExtensible {
            byte_pc,
            value_byte: u32::from(transition.slot) * 8,
        },
        vec![
            MachineOperand::location_input(object),
            MachineOperand::register_input(condition),
            MachineOperand::register_output(next),
        ],
        state_index,
        instructions,
    );
    condition = next;

    for &shape in &transition.prototype_shapes {
        let prototype = push_value(representations, MachineRepresentation::Tagged);
        let next = push_value(representations, MachineRepresentation::Boolean);
        push_guard(
            target_spec,
            MachineOpcode::CacheIrLoadPrototype { byte_pc },
            vec![
                MachineOperand::location_input(current),
                MachineOperand::register_input(condition),
                MachineOperand::register_output(prototype),
                MachineOperand::register_output(next),
            ],
            state_index,
            instructions,
        );
        current = prototype;
        condition = next;
        let next = push_value(representations, MachineRepresentation::Boolean);
        push_guard(
            target_spec,
            MachineOpcode::CacheIrGuardShape { byte_pc, shape },
            vec![
                MachineOperand::location_input(current),
                MachineOperand::register_input(condition),
                MachineOperand::register_output(next),
            ],
            state_index,
            instructions,
        );
        condition = next;
    }
    let next = push_value(representations, MachineRepresentation::Boolean);
    push_guard(
        target_spec,
        MachineOpcode::CacheIrGuardPrototypeNull { byte_pc },
        vec![
            MachineOperand::location_input(current),
            MachineOperand::register_input(condition),
            MachineOperand::register_output(next),
        ],
        state_index,
        instructions,
    );
    condition = next;

    let mut require = MachineInstruction::plain(
        MachineOpcode::GuardCondition,
        vec![MachineOperand::register_input(condition)],
    );
    require.clobbers = target_spec
        .clobbers(TargetClobberSet::StatusScratch)
        .to_vec();
    attach_frame_state(hir, machine_values, state_index, exits, &mut require);
    instructions.push(require);

    let owner = push_value(representations, MachineRepresentation::Int64);
    let stored = push_value(representations, MachineRepresentation::Boolean);
    let mut store = MachineInstruction::plain(
        MachineOpcode::CacheIrStoreField {
            byte_pc,
            value_byte: u32::from(transition.slot) * 8,
        },
        vec![
            MachineOperand::location_input(object),
            MachineOperand::location_input(value),
            MachineOperand::register_input(condition),
            MachineOperand::register_output(owner),
            MachineOperand::register_output(stored),
        ],
    );
    store.clobbers = property_store_clobbers(target_spec, false);
    store.frame_state = Some(state_index as u32);
    instructions.push(store);
    push_barrier(
        target_spec,
        byte_pc,
        owner,
        value,
        stored,
        state_index,
        instructions,
    );

    let mut publish = MachineInstruction::plain(
        MachineOpcode::CacheIrPublishShape {
            byte_pc,
            shape: transition.to_shape,
            new_len: transition.slot + 1,
            initialize_inline: transition.slot == 0,
        },
        vec![
            MachineOperand::location_input(owner),
            MachineOperand::register_input(stored),
        ],
    );
    publish.clobbers = property_store_clobbers(target_spec, false);
    publish.frame_state = Some(state_index as u32);
    instructions.push(publish);
    let shape = push_value(representations, MachineRepresentation::Tagged);
    instructions.push(MachineInstruction::plain(
        MachineOpcode::TaggedConstant(u64::from(transition.to_shape)),
        vec![MachineOperand::register_output(shape)],
    ));
    push_barrier(
        target_spec,
        byte_pc,
        owner,
        shape,
        stored,
        state_index,
        instructions,
    );
    Ok(())
}

fn push_guard(
    target_spec: &TargetSpec,
    opcode: MachineOpcode,
    operands: Vec<MachineOperand>,
    state_index: usize,
    instructions: &mut Vec<MachineInstruction>,
) {
    let mut guard = MachineInstruction::plain(opcode, operands);
    guard.clobbers = property_load_clobbers(target_spec);
    guard.frame_state = Some(state_index as u32);
    instructions.push(guard);
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
