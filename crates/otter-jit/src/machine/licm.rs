//! Effect-aware loop-invariant code motion over Machine SSA.
//!
//! # Contents
//! - Natural-loop discovery from CFG dominance.
//! - Conservative invariant selection through the shared effect table.
//! - Explicit preheader splitting shared by ordinary and OSR entry.
//!
//! # Invariants
//! - Only non-throwing, non-allocating, non-writing instructions move.
//! - A memory read moves only when the loop has no invalidating boundary or
//!   overlapping write.
//! - Loop parameters are variant. External entry and OSR execute the same
//!   preheader; generated backedges target its split body.
//! - The transform owns no raw-pointer cache or alternate semantic lowering.
//!
//! # See also
//! - `super::effects` supplies the exhaustive effect contract.
//! - `super::gvn` folds equivalences exposed by hoisting.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    ControlFlow, InstructionSequence, MachineBlock, MachineBlockData, MachineCommoning,
    MachineInstruction, MachineInstructionId, MachineOpcode, MachineValue, OperandPurpose,
    OperandRole, TargetSpec, VerificationError, effects::effects_for_instruction,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct LicmStats {
    pub(super) hoisted_instructions: u32,
    pub(super) versioned_loops: u32,
}

#[derive(Debug, Clone)]
struct NaturalLoop {
    header: usize,
    blocks: BTreeSet<usize>,
}

pub(super) fn optimize(
    mut sequence: InstructionSequence,
    target: &TargetSpec,
) -> Result<(InstructionSequence, LicmStats), VerificationError> {
    let mut stats = LicmStats::default();
    while let Some((natural_loop, candidates)) = next_loop(&sequence) {
        split_preheader_and_hoist(&mut sequence, &natural_loop, &candidates);
        stats.hoisted_instructions = stats
            .hoisted_instructions
            .saturating_add(candidates.len() as u32);
        stats.versioned_loops = stats.versioned_loops.saturating_add(1);
    }
    sequence.complete_gc_root_liveness();
    sequence.verify(target)?;
    Ok((sequence, stats))
}

fn next_loop(sequence: &InstructionSequence) -> Option<(NaturalLoop, BTreeSet<usize>)> {
    let definitions = value_definition_blocks(sequence);
    for natural_loop in innermost_natural_loops(sequence) {
        if natural_loop.header == sequence.entry.0 as usize
            || !sequence.blocks[natural_loop.header]
                .predecessors
                .iter()
                .any(|predecessor| !natural_loop.blocks.contains(&(predecessor.0 as usize)))
        {
            continue;
        }
        let candidates = invariant_instructions(sequence, &natural_loop, &definitions);
        if !candidates.is_empty() {
            return Some((natural_loop, candidates));
        }
    }
    None
}

fn invariant_instructions(
    sequence: &InstructionSequence,
    natural_loop: &NaturalLoop,
    definitions: &[Option<usize>],
) -> BTreeSet<usize> {
    let edge_values = sequence
        .blocks
        .iter()
        .flat_map(|block| block.successor_arguments.iter().flatten().copied())
        .collect::<BTreeSet<_>>();
    let mut use_blocks = vec![BTreeSet::new(); sequence.representations.len()];
    for (block_index, block) in sequence.blocks.iter().enumerate() {
        for index in block.first.0 as usize..block.end.0 as usize {
            for operand in &sequence.instructions[index].operands {
                if operand.role == OperandRole::Use {
                    use_blocks[operand.value.0 as usize].insert(block_index);
                }
            }
        }
    }
    let reconstruction_values = sequence
        .instructions
        .iter()
        .flat_map(|instruction| instruction.operands.iter())
        .filter(|operand| {
            operand.role == OperandRole::Use && operand.purpose != OperandPurpose::Input
        })
        .map(|operand| operand.value)
        .chain(sequence.frame_states.iter().flat_map(|state| {
            state.frames.iter().flat_map(|frame| {
                frame
                    .entry
                    .iter()
                    .flat_map(|entry| [&entry.new_target, &entry.this, &entry.closure])
                    .chain(frame.slots.iter())
                    .filter_map(|slot| match slot {
                        super::MachineFrameSlot::Value(value) => Some(*value),
                        super::MachineFrameSlot::TaggedLiteral(_) => None,
                    })
            })
        }))
        .collect::<BTreeSet<_>>();
    let mut writes = super::MachineAliasSet::NONE;
    let mut invalidating_boundary = false;
    for &block in &natural_loop.blocks {
        let data = &sequence.blocks[block];
        for index in data.first.0 as usize..data.end.0 as usize {
            let effects = effects_for_instruction(
                &sequence.instructions[index].opcode,
                &sequence.call_descriptors,
            );
            writes = writes.union(effects.writes);
            // Abrupt completion alone cannot change a value observed after a
            // successful continuation. Allocation, collection, or JavaScript
            // reentry can, and therefore blocks memory-proof motion.
            invalidating_boundary |= effects.allocates || effects.safepoint || effects.reentrant;
        }
    }
    let mut invariant_values = definitions
        .iter()
        .enumerate()
        .filter_map(|(value, block)| {
            block
                .is_some_and(|block| !natural_loop.blocks.contains(&block))
                .then_some(MachineValue(value as u32))
        })
        .collect::<BTreeSet<_>>();
    let loop_parameters = natural_loop
        .blocks
        .iter()
        .flat_map(|&block| sequence.blocks[block].parameters.iter().copied())
        .collect::<BTreeSet<_>>();
    let invariant_header_parameters =
        invariant_header_parameters(sequence, natural_loop, definitions);
    invariant_values.extend(invariant_header_parameters.iter().copied());
    let variant_loop_parameters = loop_parameters
        .difference(&invariant_header_parameters)
        .copied()
        .collect::<BTreeSet<_>>();
    let mut candidates = BTreeSet::new();
    loop {
        let mut changed = false;
        for &block in &natural_loop.blocks {
            let data = &sequence.blocks[block];
            for index in data.first.0 as usize..data.end.0 as usize {
                if candidates.contains(&index) {
                    continue;
                }
                let instruction = &sequence.instructions[index];
                let effects =
                    effects_for_instruction(&instruction.opcode, &sequence.call_descriptors);
                if effects.commoning == MachineCommoning::Never
                    || effects.allocates
                    || effects.throws
                    || effects.safepoint
                    || effects.reentrant
                    || !effects.writes.is_empty()
                    || effects.reads.intersects(writes)
                    || (!effects.reads.is_empty() && invalidating_boundary)
                    || instruction.control != ControlFlow::None
                    || instruction.safepoint.is_some()
                    || instruction.frame_state.is_some()
                    || !instruction.exits.is_empty()
                {
                    continue;
                }
                if !instruction.operands.iter().all(|operand| {
                    operand.role != OperandRole::Use
                        || operand.purpose != OperandPurpose::Input
                        || (invariant_values.contains(&operand.value)
                            && !variant_loop_parameters.contains(&operand.value))
                }) {
                    continue;
                }
                let outputs = instruction
                    .operands
                    .iter()
                    .filter(|operand| {
                        operand.role == OperandRole::Definition
                            && operand.purpose == OperandPurpose::Output
                    })
                    .map(|operand| operand.value)
                    .collect::<Vec<_>>();
                if outputs.is_empty() {
                    continue;
                }
                if outputs.iter().any(|output| {
                    edge_values.contains(output) || reconstruction_values.contains(output)
                }) {
                    continue;
                }
                if outputs.iter().any(|output| {
                    use_blocks[output.0 as usize]
                        .iter()
                        .any(|block| !natural_loop.blocks.contains(block))
                }) {
                    continue;
                }
                if outputs.iter().any(|output| {
                    !matches!(
                        sequence.representations[output.0 as usize],
                        super::MachineRepresentation::Int32
                            | super::MachineRepresentation::Uint32
                            | super::MachineRepresentation::Int64
                            | super::MachineRepresentation::Boolean
                    )
                }) {
                    continue;
                }
                candidates.insert(index);
                invariant_values.extend(outputs);
                changed = true;
            }
        }
        if !changed {
            return candidates;
        }
    }
}

fn invariant_header_parameters(
    sequence: &InstructionSequence,
    natural_loop: &NaturalLoop,
    definitions: &[Option<usize>],
) -> BTreeSet<MachineValue> {
    let header = &sequence.blocks[natural_loop.header];
    header
        .parameters
        .iter()
        .enumerate()
        .filter_map(|(parameter_index, &parameter)| {
            let mut saw_external = false;
            for (predecessor, block) in sequence.blocks.iter().enumerate() {
                for (edge, successor) in block.successors.iter().enumerate() {
                    if successor.0 as usize != natural_loop.header {
                        continue;
                    }
                    let argument = *block.successor_arguments.get(edge)?.get(parameter_index)?;
                    if natural_loop.blocks.contains(&predecessor) {
                        if argument != parameter {
                            return None;
                        }
                    } else {
                        saw_external = true;
                        if definitions[argument.0 as usize]
                            .is_some_and(|block| natural_loop.blocks.contains(&block))
                        {
                            return None;
                        }
                    }
                }
            }
            saw_external.then_some(parameter)
        })
        .collect()
}

fn split_preheader_and_hoist(
    sequence: &mut InstructionSequence,
    natural_loop: &NaturalLoop,
    candidates: &BTreeSet<usize>,
) {
    let header = natural_loop.header;
    let body = sequence.blocks.len();
    let old_header = sequence.blocks[header].clone();
    let old_parameters = old_header.parameters.clone();
    let new_parameters = old_parameters
        .iter()
        .map(|parameter| {
            let representation = sequence.representations[parameter.0 as usize];
            let value = MachineValue(sequence.representations.len() as u32);
            sequence.representations.push(representation);
            value
        })
        .collect::<Vec<_>>();
    let parameter_map = old_parameters
        .iter()
        .copied()
        .zip(new_parameters.iter().copied())
        .collect::<BTreeMap<_, _>>();

    let old_instructions = sequence.instructions.clone();
    let mut block_instructions = sequence
        .blocks
        .iter()
        .map(|block| old_instructions[block.first.0 as usize..block.end.0 as usize].to_vec())
        .collect::<Vec<_>>();
    let mut hoisted = Vec::new();
    for &block in &natural_loop.blocks {
        let first = sequence.blocks[block].first.0 as usize;
        let mut retained = Vec::new();
        for (offset, mut instruction) in std::mem::take(&mut block_instructions[block])
            .into_iter()
            .enumerate()
        {
            let index = first + offset;
            if block == header && matches!(instruction.opcode, MachineOpcode::OsrEntry { .. }) {
                rewrite_parameter_uses(&mut instruction, &parameter_map);
                hoisted.insert(0, instruction);
            } else if candidates.contains(&index) {
                rewrite_parameter_uses(&mut instruction, &parameter_map);
                hoisted.push(instruction);
            } else {
                retained.push(instruction);
            }
        }
        block_instructions[block] = retained;
    }
    let hoisted_outputs = hoisted
        .iter()
        .flat_map(|instruction| instruction.operands.iter())
        .filter(|operand| {
            operand.role == OperandRole::Definition && operand.purpose == OperandPurpose::Output
        })
        .map(|operand| operand.value)
        .collect::<Vec<_>>();
    let carried_outputs = hoisted_outputs
        .iter()
        .map(|output| {
            let value = MachineValue(sequence.representations.len() as u32);
            sequence
                .representations
                .push(sequence.representations[output.0 as usize]);
            (*output, value)
        })
        .collect::<BTreeMap<_, _>>();
    for &block in &natural_loop.blocks {
        for instruction in &mut block_instructions[block] {
            rewrite_parameter_uses(instruction, &carried_outputs);
        }
    }
    let header_body = std::mem::take(&mut block_instructions[header]);
    let external_predecessors = old_header
        .predecessors
        .iter()
        .copied()
        .filter(|predecessor| !natural_loop.blocks.contains(&(predecessor.0 as usize)))
        .collect::<Vec<_>>();
    let mut jump = MachineInstruction::plain(MachineOpcode::Jump, Vec::new());
    jump.control = ControlFlow::Branch;
    hoisted.push(jump);
    block_instructions[header] = hoisted;
    block_instructions.push(header_body);

    for predecessor in 0..sequence.blocks.len() {
        let block = &mut sequence.blocks[predecessor];
        for (edge, successor) in block.successors.iter_mut().enumerate() {
            if successor.0 as usize == header && natural_loop.blocks.contains(&predecessor) {
                *successor = MachineBlock(body as u32);
                block.successor_arguments[edge].extend(carried_outputs.values().copied());
            }
        }
    }
    sequence.blocks[header].predecessors = external_predecessors;
    sequence.blocks[header].successors = vec![MachineBlock(body as u32)];
    sequence.blocks[header].parameters = new_parameters.clone();
    sequence.blocks[header].successor_arguments =
        vec![new_parameters.into_iter().chain(hoisted_outputs).collect()];
    let mut body_parameters = old_parameters;
    body_parameters.extend(carried_outputs.values().copied());
    sequence.blocks.push(MachineBlockData {
        first: MachineInstructionId(0),
        end: MachineInstructionId(0),
        predecessors: Vec::new(),
        successors: old_header.successors,
        parameters: body_parameters,
        successor_arguments: old_header.successor_arguments,
    });
    rebuild_predecessors(&mut sequence.blocks);
    rebuild_instruction_ranges(sequence, block_instructions);
}

fn rewrite_parameter_uses(
    instruction: &mut MachineInstruction,
    replacements: &BTreeMap<MachineValue, MachineValue>,
) {
    for operand in &mut instruction.operands {
        if operand.role == OperandRole::Use
            && let Some(replacement) = replacements.get(&operand.value)
        {
            operand.value = *replacement;
        }
    }
}

fn rebuild_predecessors(blocks: &mut [MachineBlockData]) {
    for block in blocks.iter_mut() {
        block.predecessors.clear();
    }
    let edges = blocks
        .iter()
        .enumerate()
        .flat_map(|(predecessor, block)| {
            block
                .successors
                .iter()
                .map(move |successor| (predecessor, successor.0 as usize))
        })
        .collect::<Vec<_>>();
    for (predecessor, successor) in edges {
        blocks[successor]
            .predecessors
            .push(MachineBlock(predecessor as u32));
    }
    for block in blocks {
        block.predecessors.sort_unstable();
    }
}

fn rebuild_instruction_ranges(
    sequence: &mut InstructionSequence,
    mut block_instructions: Vec<Vec<MachineInstruction>>,
) {
    sequence.instructions.clear();
    for (block, instructions) in sequence
        .blocks
        .iter_mut()
        .zip(block_instructions.iter_mut())
    {
        block.first = MachineInstructionId(sequence.instructions.len() as u32);
        for instruction in instructions.iter_mut() {
            instruction
                .operands
                .retain(|operand| operand.purpose != OperandPurpose::TaggedRoot);
        }
        sequence.instructions.append(instructions);
        block.end = MachineInstructionId(sequence.instructions.len() as u32);
    }
}

fn value_definition_blocks(sequence: &InstructionSequence) -> Vec<Option<usize>> {
    let mut definitions = vec![None; sequence.representations.len()];
    for (block_index, block) in sequence.blocks.iter().enumerate() {
        for &parameter in &block.parameters {
            definitions[parameter.0 as usize] = Some(block_index);
        }
        for index in block.first.0 as usize..block.end.0 as usize {
            for operand in &sequence.instructions[index].operands {
                if operand.role == OperandRole::Definition
                    && operand.purpose == OperandPurpose::Output
                {
                    definitions[operand.value.0 as usize] = Some(block_index);
                }
            }
        }
    }
    definitions
}

fn innermost_natural_loops(sequence: &InstructionSequence) -> Vec<NaturalLoop> {
    let dominators = dominators(sequence);
    let mut by_header = BTreeMap::<usize, BTreeSet<usize>>::new();
    for (latch, block) in sequence.blocks.iter().enumerate() {
        for successor in &block.successors {
            let header = successor.0 as usize;
            if !dominators[latch].contains(&header) {
                continue;
            }
            let loop_blocks = by_header
                .entry(header)
                .or_insert_with(|| BTreeSet::from([header]));
            let mut stack = vec![latch];
            while let Some(block) = stack.pop() {
                if !loop_blocks.insert(block) || block == header {
                    continue;
                }
                stack.extend(
                    sequence.blocks[block]
                        .predecessors
                        .iter()
                        .map(|predecessor| predecessor.0 as usize),
                );
            }
        }
    }
    let loops = by_header
        .into_iter()
        .map(|(header, blocks)| NaturalLoop { header, blocks })
        .collect::<Vec<_>>();
    let mut innermost = loops
        .iter()
        .filter(|candidate| {
            !loops.iter().any(|other| {
                other.header != candidate.header
                    && other.blocks.len() < candidate.blocks.len()
                    && other.blocks.is_subset(&candidate.blocks)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    innermost.sort_by_key(|natural_loop| natural_loop.header);
    innermost
}

fn dominators(sequence: &InstructionSequence) -> Vec<BTreeSet<usize>> {
    let block_count = sequence.blocks.len();
    let entry = sequence.entry.0 as usize;
    let universe = (0..block_count).collect::<BTreeSet<_>>();
    let mut result = vec![universe.clone(); block_count];
    result[entry] = BTreeSet::from([entry]);
    loop {
        let mut changed = false;
        for block in 0..block_count {
            if block == entry {
                continue;
            }
            let mut predecessors = sequence.blocks[block].predecessors.iter();
            let mut next = predecessors
                .next()
                .map(|predecessor| result[predecessor.0 as usize].clone())
                .unwrap_or_default();
            for predecessor in predecessors {
                next = next
                    .intersection(&result[predecessor.0 as usize])
                    .copied()
                    .collect();
            }
            next.insert(block);
            if next != result[block] {
                result[block] = next;
                changed = true;
            }
        }
        if !changed {
            return result;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{
        MachineOperand, MachineOsrInput, MachineRepresentation, TargetClobberSet,
    };

    fn shape_guard_loop(invalidating_entry: bool) -> InstructionSequence {
        let target = TargetSpec::aarch64();
        let receiver = MachineValue(0);
        let active = MachineValue(1);
        let guarded = MachineValue(2);
        let loop_receiver = MachineValue(3);
        let mut instructions = vec![
            MachineInstruction::plain(
                MachineOpcode::EntryValue(0),
                vec![MachineOperand::register_output(receiver)],
            ),
            MachineInstruction::plain(
                MachineOpcode::BooleanConstant(true),
                vec![MachineOperand::register_output(active)],
            ),
        ];
        let mut jump = MachineInstruction::plain(MachineOpcode::Jump, vec![]);
        jump.control = ControlFlow::Branch;
        instructions.push(jump.clone());
        if invalidating_entry {
            instructions.push(MachineInstruction::plain(
                MachineOpcode::OsrEntry {
                    logical_pc: 1,
                    inputs: Vec::<MachineOsrInput>::new(),
                },
                vec![],
            ));
        }
        let mut guard = MachineInstruction::plain(
            MachineOpcode::CacheIrGuardShape {
                byte_pc: 8,
                shape: 1,
            },
            vec![
                MachineOperand::location_input(loop_receiver),
                MachineOperand::register_input(active),
                MachineOperand::register_output(guarded),
            ],
        );
        guard.clobbers = target.clobbers(TargetClobberSet::PropertyLoad).to_vec();
        instructions.push(guard);
        let mut branch = MachineInstruction::plain(
            MachineOpcode::BranchIf(false),
            vec![MachineOperand::register_input(guarded)],
        );
        branch.control = ControlFlow::Branch;
        instructions.push(branch);
        let header_end = MachineInstructionId(instructions.len() as u32);
        instructions.push(jump.clone());
        let latch_end = MachineInstructionId(instructions.len() as u32);
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(receiver)],
        );
        ret.control = ControlFlow::Return;
        instructions.push(ret);
        let end = MachineInstructionId(instructions.len() as u32);
        InstructionSequence::new(
            &target,
            MachineBlock(0),
            vec![
                MachineRepresentation::Tagged,
                MachineRepresentation::Boolean,
                MachineRepresentation::Boolean,
                MachineRepresentation::Tagged,
            ],
            vec![],
            vec![
                MachineBlockData {
                    first: MachineInstructionId(0),
                    end: MachineInstructionId(3),
                    predecessors: vec![],
                    successors: vec![MachineBlock(1)],
                    parameters: vec![],
                    successor_arguments: vec![vec![receiver]],
                },
                MachineBlockData {
                    first: MachineInstructionId(3),
                    end: header_end,
                    predecessors: vec![MachineBlock(0), MachineBlock(2)],
                    successors: vec![MachineBlock(3), MachineBlock(2)],
                    parameters: vec![loop_receiver],
                    successor_arguments: vec![vec![], vec![]],
                },
                MachineBlockData {
                    first: header_end,
                    end: latch_end,
                    predecessors: vec![MachineBlock(1)],
                    successors: vec![MachineBlock(1)],
                    parameters: vec![],
                    successor_arguments: vec![vec![loop_receiver]],
                },
                MachineBlockData {
                    first: latch_end,
                    end,
                    predecessors: vec![MachineBlock(1)],
                    successors: vec![],
                    parameters: vec![],
                    successor_arguments: vec![],
                },
            ],
            instructions,
        )
        .expect("valid shape-guard loop")
    }

    #[test]
    fn splits_a_loop_header_and_hoists_an_invariant_constant() {
        let target = TargetSpec::aarch64();
        let initial = MachineValue(0);
        let index = MachineValue(1);
        let invariant = MachineValue(2);
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
            vec![MachineOperand::register_input(invariant)],
        );
        ret.control = ControlFlow::Return;
        let sequence = InstructionSequence::new(
            &target,
            MachineBlock(0),
            vec![
                MachineRepresentation::Int32,
                MachineRepresentation::Int32,
                MachineRepresentation::Tagged,
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
                    successor_arguments: vec![vec![initial]],
                },
                MachineBlockData {
                    first: MachineInstructionId(2),
                    end: MachineInstructionId(5),
                    predecessors: vec![MachineBlock(0), MachineBlock(2)],
                    successors: vec![MachineBlock(3), MachineBlock(2)],
                    parameters: vec![index],
                    successor_arguments: vec![vec![], vec![]],
                },
                MachineBlockData {
                    first: MachineInstructionId(5),
                    end: MachineInstructionId(7),
                    predecessors: vec![MachineBlock(1)],
                    successors: vec![MachineBlock(1)],
                    parameters: vec![],
                    successor_arguments: vec![vec![next]],
                },
                MachineBlockData {
                    first: MachineInstructionId(7),
                    end: MachineInstructionId(8),
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
                MachineInstruction::plain(
                    MachineOpcode::TaggedConstant(otter_vm::Value::null().to_bits()),
                    vec![MachineOperand::register_output(invariant)],
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
        );
        let sequence = sequence.expect("valid LICM fixture");
        let (sequence, stats) = optimize(sequence, &target).expect("LICM");
        assert_eq!(stats.versioned_loops, 1);
        assert_eq!(stats.hoisted_instructions, 1);
        let preheader = &sequence.blocks[1];
        assert_eq!(preheader.successors, vec![MachineBlock(4)]);
        assert!(
            (preheader.first.0..preheader.end.0)
                .map(|index| &sequence.instructions[index as usize].opcode)
                .any(|opcode| matches!(opcode, MachineOpcode::BooleanConstant(true)))
        );
        assert_eq!(sequence.blocks[2].successors, vec![MachineBlock(4)]);
    }

    #[test]
    fn memory_guard_hoists_only_without_an_invalidating_loop_boundary() {
        let target = TargetSpec::aarch64();
        let (_, stats) = optimize(shape_guard_loop(false), &target).expect("pure guard LICM");
        assert_eq!(stats.versioned_loops, 1);
        assert_eq!(stats.hoisted_instructions, 1);

        let (sequence, stats) =
            optimize(shape_guard_loop(true), &target).expect("invalidated guard LICM");
        assert_eq!(stats, LicmStats::default());
        assert!(sequence.instructions().iter().any(|instruction| matches!(
            instruction.opcode,
            MachineOpcode::CacheIrGuardShape { .. }
        )));
    }
}
