//! Trivial block-parameter elimination over Machine SSA.
//!
//! # Contents
//! - [`optimize`] removes every block parameter whose incoming arguments,
//!   ignoring the parameter itself, name one single value, to a fixed point,
//!   as in Braun et al. "Simple and Efficient SSA Construction" (the
//!   `tryRemoveTrivialPhi` step V8's and Cranelift's builders rely on).
//!   Loop headers receive a parameter for every live interpreter register
//!   before the back edge is known; for a register the loop never writes
//!   that parameter only forwards its entry value. Such parameters dominate
//!   large straight-line asm.js-style bodies, where each one costs a
//!   register-allocator live range, block-edge moves, and blocks that cannot
//!   merge.
//!
//! # Invariants
//! - A parameter is replaced only when every non-self incoming argument
//!   resolves to the same value of the same representation. That value is
//!   available on every entering path, so it dominates the block and every
//!   former use of the parameter.
//! - Landing-pad parameters are defined by exceptional edges, not by block
//!   arguments, and are never replaced.
//! - Every use site is rewritten: instruction operands, block arguments,
//!   inline frames, frame states, and virtual-object fields. An instruction
//!   lists each frame-state value once, so operands that now name one value
//!   collapse. Tagged GC root operands are liveness-derived and are
//!   recomputed afterward.
//! - No instruction is added, removed, or reordered.
//!
//! # See also
//! - `super::block_merge` joins blocks that lose their last parameter here.
//! - `super::dce` follows the same root-recomputation contract.

use rustc_hash::FxHashSet;

use super::{
    ExceptionalEdge, InstructionSequence, MachineFrameSlot, MachineValue, OperandPurpose,
    OperandRole, TargetSpec, VerificationError,
};

pub(super) fn optimize(
    mut sequence: InstructionSequence,
    target: &TargetSpec,
) -> Result<(InstructionSequence, u32), VerificationError> {
    let landing_pads = sequence
        .call_descriptors
        .iter()
        .filter_map(|descriptor| match descriptor.exceptional {
            ExceptionalEdge::LandingPad(block) => Some(block.0 as usize),
            ExceptionalEdge::None | ExceptionalEdge::Propagate => None,
        })
        .collect::<FxHashSet<_>>();
    let mut replacement = (0..sequence.representations.len() as u32)
        .map(MachineValue)
        .collect::<Vec<_>>();
    let mut removed = sequence
        .blocks
        .iter()
        .map(|block| vec![false; block.parameters.len()])
        .collect::<Vec<_>>();
    let mut eliminated = 0u32;
    let mut changed = true;
    while changed {
        changed = false;
        for (block_index, block) in sequence.blocks.iter().enumerate() {
            if landing_pads.contains(&block_index) || block.predecessors.is_empty() {
                continue;
            }
            for (parameter_index, &parameter) in block.parameters.iter().enumerate() {
                if removed[block_index][parameter_index] {
                    continue;
                }
                let Some(value) = single_incoming(
                    &sequence,
                    &mut replacement,
                    block_index,
                    parameter_index,
                    parameter,
                ) else {
                    continue;
                };
                if sequence.representations.get(value.0 as usize)
                    != sequence.representations.get(parameter.0 as usize)
                {
                    continue;
                }
                replacement[parameter.0 as usize] = value;
                removed[block_index][parameter_index] = true;
                eliminated += 1;
                changed = true;
            }
        }
    }
    if eliminated == 0 {
        return Ok((sequence, 0));
    }

    for value in 0..replacement.len() {
        resolve(&mut replacement, MachineValue(value as u32));
    }
    let map = |value: MachineValue| replacement[value.0 as usize];

    // Drop removed parameters and the matching argument at every edge into
    // their block before rewriting surviving arguments.
    for block_index in 0..sequence.blocks.len() {
        let successors = sequence.blocks[block_index].successors.clone();
        for (edge, successor) in successors.iter().enumerate() {
            let dropped = &removed[successor.0 as usize];
            if !dropped.contains(&true) {
                continue;
            }
            let arguments = &mut sequence.blocks[block_index].successor_arguments[edge];
            let mut index = 0;
            arguments.retain(|_| {
                let keep = !dropped[index];
                index += 1;
                keep
            });
        }
    }
    for (block, dropped) in sequence.blocks.iter_mut().zip(&removed) {
        let mut index = 0;
        block.parameters.retain(|_| {
            let keep = !dropped[index];
            index += 1;
            keep
        });
        for value in block.successor_arguments.iter_mut().flatten() {
            *value = map(*value);
        }
    }
    for instruction in &mut sequence.instructions {
        instruction
            .operands
            .retain(|operand| operand.purpose != OperandPurpose::TaggedRoot);
        for operand in &mut instruction.operands {
            if operand.role == OperandRole::Use {
                operand.value = map(operand.value);
            }
        }
        // Two registers that carried distinct parameters may now name one
        // value; a frame state lists each value once.
        if instruction
            .operands
            .iter()
            .any(|operand| operand.purpose == OperandPurpose::FrameState)
        {
            let mut seen = FxHashSet::default();
            instruction.operands.retain(|operand| {
                operand.purpose != OperandPurpose::FrameState || seen.insert(operand.value)
            });
        }
        for frame in instruction.inline_frames.iter_mut() {
            if let Some(entry) = &mut frame.entry {
                for slot in [&mut entry.this, &mut entry.closure, &mut entry.new_target] {
                    *slot = slot.map(map);
                }
            }
            for slot in frame.slots.iter_mut() {
                *slot = slot.map(map);
            }
        }
    }
    let remap_slot = |slot: &mut MachineFrameSlot| {
        if let MachineFrameSlot::Value(value) = slot {
            *value = map(*value);
        }
    };
    for state in &mut sequence.frame_states {
        for frame in state.frames.iter_mut() {
            if let Some(entry) = &mut frame.entry {
                remap_slot(&mut entry.this);
                remap_slot(&mut entry.closure);
                remap_slot(&mut entry.new_target);
            }
            frame.slots.iter_mut().for_each(remap_slot);
        }
        for object in state.virtual_objects.iter_mut() {
            object.fields.iter_mut().for_each(remap_slot);
        }
    }
    sequence.complete_gc_root_liveness();
    sequence.verify_pass(target)?;
    Ok((sequence, eliminated))
}

/// The one value, other than `parameter` itself, that every edge into
/// `block_index` passes for `parameter_index`, if there is exactly one.
fn single_incoming(
    sequence: &InstructionSequence,
    replacement: &mut [MachineValue],
    block_index: usize,
    parameter_index: usize,
    parameter: MachineValue,
) -> Option<MachineValue> {
    let mut single = None;
    for predecessor in &sequence.blocks[block_index].predecessors {
        let predecessor = sequence.blocks.get(predecessor.0 as usize)?;
        for (edge, successor) in predecessor.successors.iter().enumerate() {
            if successor.0 as usize != block_index {
                continue;
            }
            let argument = *predecessor
                .successor_arguments
                .get(edge)?
                .get(parameter_index)?;
            let value = resolve(replacement, argument);
            if value == parameter {
                continue;
            }
            match single {
                None => single = Some(value),
                Some(previous) if previous == value => {}
                Some(_) => return None,
            }
        }
    }
    single
}

/// Follow replacements to the surviving value, compressing the path.
fn resolve(replacement: &mut [MachineValue], value: MachineValue) -> MachineValue {
    let mut root = value;
    while replacement[root.0 as usize] != root {
        root = replacement[root.0 as usize];
    }
    let mut current = value;
    while replacement[current.0 as usize] != root {
        let next = replacement[current.0 as usize];
        replacement[current.0 as usize] = root;
        current = next;
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{
        ControlFlow, MachineBlock, MachineBlockData, MachineInstruction, MachineInstructionId,
        MachineOpcode, MachineOperand, MachineRepresentation,
    };

    #[test]
    fn a_loop_invariant_header_parameter_forwards_its_entry_value() {
        let target = TargetSpec::aarch64();
        let initial = MachineValue(0);
        let index = MachineValue(1);
        let carried = MachineValue(2);
        let condition = MachineValue(3);
        let next = MachineValue(4);
        let jump = || {
            let mut instruction = MachineInstruction::plain(MachineOpcode::Jump, vec![]);
            instruction.control = ControlFlow::Branch;
            instruction
        };
        let mut branch = MachineInstruction::plain(
            MachineOpcode::BranchIf(false),
            vec![MachineOperand::register_input(condition)],
        );
        branch.control = ControlFlow::Branch;
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(carried)],
        );
        ret.control = ControlFlow::Return;
        let sequence = InstructionSequence::new(
            &target,
            MachineBlock(0),
            vec![
                MachineRepresentation::Int32,
                MachineRepresentation::Int32,
                MachineRepresentation::Int32,
                MachineRepresentation::Boolean,
                MachineRepresentation::Int32,
            ],
            vec![],
            vec![
                MachineBlockData {
                    first: MachineInstructionId(0),
                    end: MachineInstructionId(2),
                    predecessors: vec![],
                    successors: vec![MachineBlock(1)],
                    parameters: vec![],
                    successor_arguments: vec![vec![initial, initial]],
                },
                MachineBlockData {
                    first: MachineInstructionId(2),
                    end: MachineInstructionId(4),
                    predecessors: vec![MachineBlock(0), MachineBlock(2)],
                    successors: vec![MachineBlock(3), MachineBlock(2)],
                    parameters: vec![index, carried],
                    successor_arguments: vec![vec![], vec![]],
                },
                MachineBlockData {
                    first: MachineInstructionId(4),
                    end: MachineInstructionId(6),
                    predecessors: vec![MachineBlock(1)],
                    successors: vec![MachineBlock(1)],
                    parameters: vec![],
                    successor_arguments: vec![vec![next, carried]],
                },
                MachineBlockData {
                    first: MachineInstructionId(6),
                    end: MachineInstructionId(7),
                    predecessors: vec![MachineBlock(1)],
                    successors: vec![],
                    parameters: vec![],
                    successor_arguments: vec![],
                },
            ],
            vec![
                MachineInstruction::plain(
                    MachineOpcode::IntegerConstant(0),
                    vec![MachineOperand::register_output(initial)],
                ),
                jump(),
                MachineInstruction::plain(
                    MachineOpcode::BooleanConstant(true),
                    vec![MachineOperand::register_output(condition)],
                ),
                branch,
                MachineInstruction::plain(
                    MachineOpcode::IntegerAddImmediate(1),
                    vec![
                        MachineOperand::register_input(index),
                        MachineOperand::register_output(next),
                    ],
                ),
                jump(),
                ret,
            ],
        )
        .expect("valid loop fixture");
        let (sequence, eliminated) = optimize(sequence, &target).expect("trivial phi");
        assert_eq!(eliminated, 1);
        assert_eq!(sequence.blocks[1].parameters, vec![index]);
        assert_eq!(sequence.blocks[0].successor_arguments, vec![vec![initial]]);
        assert_eq!(sequence.blocks[2].successor_arguments, vec![vec![next]]);
        assert_eq!(sequence.instructions[6].operands[0].value, initial);
    }
}
