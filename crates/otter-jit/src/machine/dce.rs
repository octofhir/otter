//! Dead pure-instruction elimination over Machine SSA.
//!
//! # Contents
//! - [`optimize`] removes every side-effect-free instruction whose outputs no
//!   operand, block argument, frame state or inline frame reads, to a fixed
//!   point, so a value chain abandoned by a rewrite (a float computation
//!   replaced by its int32 truncation, a boxed copy nothing reads) costs no
//!   code.
//!
//! # Invariants
//! - Only instructions without control flow, exits, safepoints, frame states,
//!   writes, allocation, throwing or reentry are removed; an identity-bearing
//!   (`MachineCommoning::Never`) instruction or an entry/OSR mapping stays.
//! - Every frame state value counts as a use, so deoptimization still
//!   reconstructs exactly the values it names.
//! - Block order and every remaining instruction's order are unchanged.
//!
//! # See also
//! - `super::gvn` removes redundant instructions; this pass removes unread ones.
//! - `super::effects` owns the effect contract consulted here.

use super::{
    ControlFlow, InstructionSequence, MachineCommoning, MachineFrameSlot, MachineInstruction,
    MachineInstructionId, MachineOpcode, OperandPurpose, OperandRole, TargetSpec,
    VerificationError, effects::effects_for_instruction,
};

pub(super) fn optimize(
    mut sequence: InstructionSequence,
    target: &TargetSpec,
) -> Result<(InstructionSequence, u32), VerificationError> {
    let mut uses = vec![0u32; sequence.representations.len()];
    let count = |value: super::MachineValue, uses: &mut Vec<u32>| {
        if let Some(slot) = uses.get_mut(value.0 as usize) {
            *slot = slot.saturating_add(1);
        }
    };
    // GC root operands are derived from liveness and recomputed below, so
    // they never keep a value alive.
    let mut producer = vec![usize::MAX; sequence.representations.len()];
    for (index, instruction) in sequence.instructions.iter().enumerate() {
        for operand in &instruction.operands {
            if operand.role == OperandRole::Use && operand.purpose != OperandPurpose::TaggedRoot {
                count(operand.value, &mut uses);
            }
            if operand.purpose == OperandPurpose::Output
                && let Some(slot) = producer.get_mut(operand.value.0 as usize)
            {
                *slot = index;
            }
        }
        for frame in &instruction.inline_frames {
            for value in frame
                .entry
                .iter()
                .flat_map(|entry| [&entry.new_target, &entry.this, &entry.closure])
                .chain(frame.slots.iter())
                .flatten()
            {
                count(*value, &mut uses);
            }
        }
    }
    for block in &sequence.blocks {
        for value in block.successor_arguments.iter().flatten() {
            count(*value, &mut uses);
        }
    }
    for state in &sequence.frame_states {
        let slots = state
            .frames
            .iter()
            .flat_map(|frame| {
                frame
                    .entry
                    .iter()
                    .flat_map(|entry| [&entry.new_target, &entry.this, &entry.closure])
                    .chain(frame.slots.iter())
            })
            .chain(
                state
                    .virtual_objects
                    .iter()
                    .flat_map(|object| object.fields.iter()),
            );
        for slot in slots {
            if let MachineFrameSlot::Value(value) = slot {
                count(*value, &mut uses);
            }
        }
    }

    let mut removed = vec![false; sequence.instructions.len()];
    let mut worklist = (0..sequence.instructions.len()).rev().collect::<Vec<_>>();
    while let Some(index) = worklist.pop() {
        if removed[index] || !removable(&sequence, &sequence.instructions[index]) {
            continue;
        }
        let instruction = &sequence.instructions[index];
        let outputs_unread = instruction
            .operands
            .iter()
            .filter(|operand| operand.purpose == OperandPurpose::Output)
            .all(|operand| uses[operand.value.0 as usize] == 0);
        if !outputs_unread {
            continue;
        }
        removed[index] = true;
        for operand in &instruction.operands {
            if operand.role != OperandRole::Use || operand.purpose == OperandPurpose::TaggedRoot {
                continue;
            }
            let slot = &mut uses[operand.value.0 as usize];
            *slot = slot.saturating_sub(1);
            // The value's producer may now be dead as well.
            if *slot == 0 && producer[operand.value.0 as usize] != usize::MAX {
                worklist.push(producer[operand.value.0 as usize]);
            }
        }
    }
    let eliminated = removed.iter().filter(|removed| **removed).count() as u32;
    if eliminated == 0 {
        return Ok((sequence, 0));
    }
    let capacity = sequence.instructions.len() - eliminated as usize;
    let mut old = std::mem::take(&mut sequence.instructions)
        .into_iter()
        .zip(removed);
    let mut kept = Vec::with_capacity(capacity);
    for block in &mut sequence.blocks {
        // Verified blocks own contiguous ranges in block order, so the
        // instructions move out without a copy.
        let len = (block.end.0 - block.first.0) as usize;
        block.first = MachineInstructionId(kept.len() as u32);
        for (mut instruction, removed) in old.by_ref().take(len) {
            if removed {
                continue;
            }
            instruction
                .operands
                .retain(|operand| operand.purpose != OperandPurpose::TaggedRoot);
            kept.push(instruction);
        }
        block.end = MachineInstructionId(kept.len() as u32);
    }
    sequence.instructions = kept;
    super::renumber_safepoints(&mut sequence.instructions);
    sequence.complete_gc_root_liveness();
    sequence.verify_pass(target)?;
    Ok((sequence, eliminated))
}

fn removable(sequence: &InstructionSequence, instruction: &MachineInstruction) -> bool {
    if instruction.control != ControlFlow::None
        || !instruction.exits.is_empty()
        || instruction.safepoint.is_some()
        || instruction.frame_state.is_some()
        || !instruction.inline_frames.is_empty()
        || matches!(
            instruction.opcode,
            MachineOpcode::EntryValue(_)
                | MachineOpcode::EntryThis
                | MachineOpcode::OsrValue { .. }
                | MachineOpcode::OsrDispatch { .. }
                | MachineOpcode::LoopPreheader
        )
    {
        return false;
    }
    let effects = effects_for_instruction(&instruction.opcode, &sequence.call_descriptors);
    effects.writes.is_empty()
        && !effects.allocates
        && !effects.throws
        && !effects.safepoint
        && !effects.reentrant
        && effects.commoning != MachineCommoning::Never
}
