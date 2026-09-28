//! Effect-aware loop-invariant code motion over Machine SSA.
//!
//! # Contents
//! - Natural-loop discovery from the shared dominator tree
//!   (`super::dominance`).
//! - One analysis per innermost loop, discovered once: splitting a loop
//!   appends its body block and leaves every other innermost loop's blocks and
//!   invariants unchanged.
//! - Conservative invariant selection through the shared effect table.
//! - Explicit preheader splitting shared by ordinary and OSR entry.
//!
//! # Invariants
//! - Only non-throwing, non-allocating, non-writing instructions move.
//!   Their results are scalar, except SELF and context-chain reads, whose
//!   tagged results are ordinary rooted values addressed from their input.
//! - The preheader holds hoisted instructions in dependency order: block
//!   indices stop following dominance once splits append body blocks.
//! - A memory read moves only when the loop has no invalidating boundary or
//!   overlapping write.
//! - Loop parameters are variant unless every backedge passes them unchanged.
//!   Every external entry, ordinary and OSR block alike, executes the same
//!   preheader; generated backedges target its split body.
//! - Safepoint ids are renumbered in the new instruction order after every
//!   split, because the split body block is appended after all others.
//! - The transform owns no raw-pointer cache or alternate semantic lowering.
//!
//! # See also
//! - `super::effects` supplies the exhaustive effect contract.
//! - `super::gvn` folds equivalences exposed by hoisting.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    ControlFlow, InstructionSequence, MachineBlock, MachineBlockData, MachineCommoning,
    MachineInstruction, MachineInstructionId, MachineOpcode, MachineValue, OperandPurpose,
    OperandRole, TargetSpec, VerificationError, dominance::Dominance,
    effects::effects_for_instruction,
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
    // Innermost loops are disjoint, so splitting one never changes another
    // loop's blocks or invariants, and block indices stay stable because a
    // split only appends the new body block. Every loop's invariants are
    // therefore planned against the original sequence.
    let mut facts: Option<ValueFacts> = None;
    let mut plans = Vec::new();
    for natural_loop in innermost_natural_loops(&sequence) {
        if natural_loop.header == sequence.entry.0 as usize
            || !sequence.blocks[natural_loop.header]
                .predecessors
                .iter()
                .any(|predecessor| !natural_loop.blocks.contains(&(predecessor.0 as usize)))
        {
            continue;
        }
        let facts = facts.get_or_insert_with(|| ValueFacts::compute(&sequence));
        let candidates = invariant_instructions(&sequence, &natural_loop, facts);
        if !candidates.is_empty() {
            plans.push((natural_loop, candidates));
        }
    }
    if plans.is_empty() {
        return Ok((sequence, stats));
    }
    // Every split reads its loop's original blocks and instruction indices
    // and appends one body block, so all splits apply to one partition of
    // the instructions and the ranges are rebuilt once.
    let mut old_instructions = std::mem::take(&mut sequence.instructions).into_iter();
    let mut block_instructions = sequence
        .blocks
        .iter()
        .map(|block| {
            old_instructions
                .by_ref()
                .take((block.end.0 - block.first.0) as usize)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    for (natural_loop, candidates) in &plans {
        split_preheader_and_hoist(
            &mut sequence,
            &mut block_instructions,
            natural_loop,
            candidates,
        );
        stats.hoisted_instructions = stats
            .hoisted_instructions
            .saturating_add(candidates.len() as u32);
        stats.versioned_loops = stats.versioned_loops.saturating_add(1);
    }
    rebuild_predecessors(&mut sequence.blocks);
    rebuild_instruction_ranges(&mut sequence, block_instructions);
    sequence.complete_gc_root_liveness();
    sequence.verify_pass(target)?;
    Ok((sequence, stats))
}

/// Function-wide value facts shared by every loop analyzed against one CFG.
struct ValueFacts {
    definitions: Vec<Option<usize>>,
    /// Values passed along a CFG edge.
    edge_values: Vec<bool>,
    /// Values read by a non-input operand or by deopt reconstruction.
    reconstruction_values: Vec<bool>,
    /// Input-operand reads of each value across the whole function.
    input_uses: Vec<u32>,
}

impl ValueFacts {
    fn compute(sequence: &InstructionSequence) -> Self {
        let count = sequence.representations.len();
        let mut edge_values = vec![false; count];
        for value in sequence
            .blocks
            .iter()
            .flat_map(|block| block.successor_arguments.iter().flatten())
        {
            edge_values[value.0 as usize] = true;
        }
        let mut reconstruction_values = vec![false; count];
        let frame_values = sequence.frame_states.iter().flat_map(|state| {
            state
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
                )
        });
        for value in sequence
            .instructions
            .iter()
            .flat_map(|instruction| instruction.operands.iter())
            .filter(|operand| {
                operand.role == OperandRole::Use && operand.purpose != OperandPurpose::Input
            })
            .map(|operand| operand.value)
            .chain(frame_values.filter_map(|slot| match slot {
                super::MachineFrameSlot::Value(value) => Some(*value),
                super::MachineFrameSlot::TaggedLiteral(_)
                | super::MachineFrameSlot::VirtualObject(_) => None,
            }))
        {
            reconstruction_values[value.0 as usize] = true;
        }
        let mut input_uses = vec![0u32; count];
        for operand in sequence
            .instructions
            .iter()
            .flat_map(|instruction| instruction.operands.iter())
            .filter(|operand| {
                operand.role == OperandRole::Use && operand.purpose == OperandPurpose::Input
            })
        {
            input_uses[operand.value.0 as usize] += 1;
        }
        Self {
            definitions: value_definition_blocks(sequence),
            edge_values,
            reconstruction_values,
            input_uses,
        }
    }
}

fn invariant_instructions(
    sequence: &InstructionSequence,
    natural_loop: &NaturalLoop,
    facts: &ValueFacts,
) -> BTreeSet<usize> {
    let definitions = &facts.definitions;
    let mut in_loop = vec![false; sequence.blocks.len()];
    for &block in &natural_loop.blocks {
        in_loop[block] = true;
    }
    // A value read outside the loop is read more often in the function than
    // inside the loop; every non-input read already counts as reconstruction.
    let mut loop_input_uses = rustc_hash::FxHashMap::<u32, u32>::default();
    for &block in &natural_loop.blocks {
        let data = &sequence.blocks[block];
        for operand in sequence.instructions[data.first.0 as usize..data.end.0 as usize]
            .iter()
            .flat_map(|instruction| instruction.operands.iter())
            .filter(|operand| {
                operand.role == OperandRole::Use && operand.purpose == OperandPurpose::Input
            })
        {
            *loop_input_uses.entry(operand.value.0).or_default() += 1;
        }
    }
    let used_outside = |value: MachineValue| {
        facts.input_uses[value.0 as usize] > loop_input_uses.get(&value.0).copied().unwrap_or(0)
    };
    let mut writes = super::MachineAliasSet::NONE;
    let mut invalidating_boundary = false;
    let mut reentrant_boundary = false;
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
            reentrant_boundary |= effects.reentrant;
        }
    }
    let invariant_header_parameters =
        invariant_header_parameters(sequence, natural_loop, definitions);
    let mut hoisted_outputs = rustc_hash::FxHashSet::<MachineValue>::default();
    let mut variant_loop_parameters = rustc_hash::FxHashSet::<MachineValue>::default();
    for &block in &natural_loop.blocks {
        for parameter in &sequence.blocks[block].parameters {
            if !invariant_header_parameters.contains(parameter) {
                variant_loop_parameters.insert(*parameter);
            }
        }
    }
    let invariant = |value: MachineValue, hoisted: &rustc_hash::FxHashSet<MachineValue>| {
        !variant_loop_parameters.contains(&value)
            && (definitions[value.0 as usize].is_some_and(|block| !in_loop[block])
                || invariant_header_parameters.contains(&value)
                || hoisted.contains(&value))
    };
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
                // Binding and in-heap element-view proofs yield raw heap
                // addresses: keep them inside one iteration even in a
                // non-reentrant loop. A typed-array view's base is off-heap
                // storage that a collection never moves; only JavaScript
                // reentry (detach, resize, transfer) can change its proof, so
                // it moves out of any loop that cannot reenter.
                let off_heap_view = instruction.opcode.is_off_heap_element_view();
                if (matches!(
                    instruction.opcode,
                    MachineOpcode::BindingGuard { .. } | MachineOpcode::ElementView { .. }
                ) && !off_heap_view)
                    || effects.commoning == MachineCommoning::Never
                    || effects.allocates
                    || effects.throws
                    || effects.safepoint
                    || effects.reentrant
                    || !effects.writes.is_empty()
                    || effects.reads.intersects(writes)
                    || (!effects.reads.is_empty()
                        && if off_heap_view {
                            reentrant_boundary
                        } else {
                            invalidating_boundary
                        })
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
                        || invariant(operand.value, &hoisted_outputs)
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
                    facts.edge_values[output.0 as usize]
                        || facts.reconstruction_values[output.0 as usize]
                        || used_outside(*output)
                }) {
                    continue;
                }
                // Tagged results move only for context-chain reads: their
                // tagged input is the owning heap value itself, so a hoisted
                // result is an ordinary rooted value and no derived address
                // crosses a backedge or a safepoint.
                let tagged_movable = matches!(
                    instruction.opcode,
                    MachineOpcode::EntryCallee | MachineOpcode::ContextLoad { .. }
                );
                if outputs
                    .iter()
                    .any(|output| match sequence.representations[output.0 as usize] {
                        super::MachineRepresentation::Int32
                        | super::MachineRepresentation::Uint32
                        | super::MachineRepresentation::Int64
                        | super::MachineRepresentation::Boolean => false,
                        super::MachineRepresentation::Tagged => !tagged_movable,
                        _ => true,
                    })
                {
                    continue;
                }
                candidates.insert(index);
                hoisted_outputs.extend(outputs);
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
    // Only the header's predecessors carry edges into it.
    let predecessors = header
        .predecessors
        .iter()
        .map(|predecessor| predecessor.0 as usize)
        .collect::<BTreeSet<_>>();
    header
        .parameters
        .iter()
        .enumerate()
        .filter_map(|(parameter_index, &parameter)| {
            let mut saw_external = false;
            for &predecessor in &predecessors {
                let block = &sequence.blocks[predecessor];
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

/// Order hoisted invariants so every definition precedes its uses.
///
/// Candidates are gathered block by block, and block indices follow creation
/// rather than dominance once earlier splits have appended body blocks, so a
/// use can be gathered ahead of the invariant that defines its operand. The
/// order is otherwise the gathered one (Kahn's algorithm, ready set ordered by
/// gathering position), keeping the transform deterministic.
fn in_dependency_order(invariants: Vec<MachineInstruction>) -> Vec<MachineInstruction> {
    let mut producer = BTreeMap::new();
    for (position, instruction) in invariants.iter().enumerate() {
        for operand in &instruction.operands {
            if operand.role == OperandRole::Definition && operand.purpose == OperandPurpose::Output
            {
                producer.insert(operand.value, position);
            }
        }
    }
    let mut pending = vec![0usize; invariants.len()];
    let mut dependents = vec![Vec::new(); invariants.len()];
    for (position, instruction) in invariants.iter().enumerate() {
        let inputs = instruction
            .operands
            .iter()
            .filter(|operand| operand.role == OperandRole::Use)
            .filter_map(|operand| producer.get(&operand.value).copied())
            .filter(|&source| source != position)
            .collect::<BTreeSet<_>>();
        pending[position] = inputs.len();
        for source in inputs {
            dependents[source].push(position);
        }
    }
    let mut ready = (0..invariants.len())
        .filter(|&position| pending[position] == 0)
        .collect::<BTreeSet<_>>();
    let mut slots = invariants.into_iter().map(Some).collect::<Vec<_>>();
    let mut ordered = Vec::with_capacity(slots.len());
    while let Some(position) = ready.pop_first() {
        ordered.push(
            slots[position]
                .take()
                .expect("each invariant is emitted once"),
        );
        for &dependent in &dependents[position] {
            pending[dependent] -= 1;
            if pending[dependent] == 0 {
                ready.insert(dependent);
            }
        }
    }
    debug_assert!(
        slots.iter().all(Option::is_none),
        "loop invariants cannot form a dependency cycle"
    );
    ordered
}

fn split_preheader_and_hoist(
    sequence: &mut InstructionSequence,
    block_instructions: &mut Vec<Vec<MachineInstruction>>,
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

    let mut invariants = Vec::new();
    for &block in &natural_loop.blocks {
        let first = sequence.blocks[block].first.0 as usize;
        let mut retained = Vec::new();
        for (offset, mut instruction) in std::mem::take(&mut block_instructions[block])
            .into_iter()
            .enumerate()
        {
            let index = first + offset;
            if candidates.contains(&index) {
                rewrite_parameter_uses(&mut instruction, &parameter_map);
                invariants.push(instruction);
            } else {
                retained.push(instruction);
            }
        }
        block_instructions[block] = retained;
    }
    let mut hoisted = in_dependency_order(invariants);
    let hoisted_outputs = hoisted
        .iter()
        .flat_map(|instruction| instruction.operands.iter())
        .filter(|operand| {
            operand.role == OperandRole::Definition && operand.purpose == OperandPurpose::Output
        })
        .map(|operand| operand.value)
        .collect::<Vec<_>>();
    let carried_output_parameters = hoisted_outputs
        .iter()
        .map(|output| {
            let value = MachineValue(sequence.representations.len() as u32);
            sequence
                .representations
                .push(sequence.representations[output.0 as usize]);
            (*output, value)
        })
        .collect::<Vec<_>>();
    let carried_outputs = carried_output_parameters
        .iter()
        .copied()
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
                block.successor_arguments[edge]
                    .extend(carried_output_parameters.iter().map(|(_, value)| *value));
            }
        }
    }
    sequence.blocks[header].predecessors = external_predecessors;
    sequence.blocks[header].successors = vec![MachineBlock(body as u32)];
    sequence.blocks[header].parameters = new_parameters.clone();
    sequence.blocks[header].successor_arguments =
        vec![new_parameters.into_iter().chain(hoisted_outputs).collect()];
    let mut body_parameters = old_parameters;
    body_parameters.extend(carried_output_parameters.iter().map(|(_, value)| *value));
    sequence.blocks.push(MachineBlockData {
        first: MachineInstructionId(0),
        end: MachineInstructionId(0),
        predecessors: Vec::new(),
        successors: old_header.successors,
        parameters: body_parameters,
        successor_arguments: old_header.successor_arguments,
    });
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
    // The split body now follows every other block in the array.
    super::renumber_safepoints(&mut sequence.instructions);
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
    let dominance = Dominance::compute(sequence.blocks.len(), sequence.entry.0 as usize, |block| {
        sequence.blocks[block]
            .successors
            .iter()
            .map(|successor| successor.0 as usize)
    });
    let mut by_header = BTreeMap::<usize, BTreeSet<usize>>::new();
    for (latch, block) in sequence.blocks.iter().enumerate() {
        for successor in &block.successors {
            let header = successor.0 as usize;
            if !dominance.dominates(header, latch) {
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
    // A loop nested in `candidate` has its header inside `candidate`, so only
    // those loops need the subset test.
    loops
        .iter()
        .filter(|candidate| {
            !loops.iter().any(|other| {
                other.header != candidate.header
                    && candidate.blocks.contains(&other.header)
                    && other.blocks.len() < candidate.blocks.len()
                    && other.blocks.is_subset(&candidate.blocks)
            })
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{MachineOperand, MachineOsrType, MachineRepresentation, TargetClobberSet};

    #[test]
    fn hoisted_invariants_define_before_they_use() {
        let constant = MachineValue(1);
        let truth = MachineValue(2);
        let shifted = MachineValue(3);
        let outer = MachineValue(0);
        // Gathered as a later split's body block would leave them: both uses
        // of the constant ahead of its definition.
        let gathered = vec![
            MachineInstruction::plain(
                MachineOpcode::IntegerToBoolean,
                vec![
                    MachineOperand::register_input(constant),
                    MachineOperand::register_output(truth),
                ],
            ),
            MachineInstruction::plain(
                MachineOpcode::IntegerShiftRightLogical,
                vec![
                    MachineOperand::register_input(outer),
                    MachineOperand::register_input(constant),
                    MachineOperand::register_output(shifted),
                ],
            ),
            MachineInstruction::plain(
                MachineOpcode::IntegerConstant(0),
                vec![MachineOperand::register_output(constant)],
            ),
        ];
        let ordered = in_dependency_order(gathered)
            .into_iter()
            .map(|instruction| instruction.opcode)
            .collect::<Vec<_>>();
        assert_eq!(
            ordered,
            [
                MachineOpcode::IntegerConstant(0),
                MachineOpcode::IntegerToBoolean,
                MachineOpcode::IntegerShiftRightLogical,
            ]
        );
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum LoopBoundary {
        None,
        LatchShapeWrite,
    }

    fn shape_guard_loop(boundary: LoopBoundary) -> InstructionSequence {
        let target = TargetSpec::aarch64();
        let receiver = MachineValue(0);
        let active = MachineValue(1);
        let guarded = MachineValue(2);
        let loop_receiver = MachineValue(3);
        let owner = MachineValue(4);
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
        if boundary == LoopBoundary::LatchShapeWrite {
            instructions.push(MachineInstruction::plain(
                MachineOpcode::IntegerConstant(0),
                vec![MachineOperand::register_output(owner)],
            ));
            let mut publish = MachineInstruction::plain(
                MachineOpcode::CacheIrPublishShape {
                    byte_pc: 9,
                    shape: 2,
                    new_len: 1,
                    initialize_inline: true,
                },
                vec![
                    MachineOperand::location_input(owner),
                    MachineOperand::register_input(active),
                ],
            );
            publish.clobbers = target.clobbers(TargetClobberSet::PropertyStore).to_vec();
            instructions.push(publish);
        }
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
                MachineRepresentation::Int64,
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
            vec![MachineOperand::register_input(initial)],
        );
        ret.control = ControlFlow::Return;
        let sequence = InstructionSequence::new(
            &target,
            MachineBlock(0),
            vec![
                MachineRepresentation::Int32,
                MachineRepresentation::Int32,
                MachineRepresentation::Boolean,
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
                    MachineOpcode::BooleanConstant(false),
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
        assert_eq!(stats.hoisted_instructions, 2);
        let preheader = &sequence.blocks[1];
        assert_eq!(preheader.successors, vec![MachineBlock(4)]);
        assert!(
            (preheader.first.0..preheader.end.0)
                .map(|index| &sequence.instructions[index as usize].opcode)
                .any(|opcode| matches!(opcode, MachineOpcode::BooleanConstant(true)))
        );
        assert_eq!(sequence.blocks[2].successors, vec![MachineBlock(4)]);
        let body = &sequence.blocks[4];
        let branch = (body.first.0..body.end.0)
            .map(|index| &sequence.instructions[index as usize])
            .find(|instruction| matches!(instruction.opcode, MachineOpcode::BranchIf(false)))
            .expect("loop body branch");
        assert_eq!(branch.operands[0].value, body.parameters[1]);
    }

    #[test]
    fn memory_guard_hoists_only_without_an_invalidating_loop_boundary() {
        let target = TargetSpec::aarch64();
        let (_, stats) =
            optimize(shape_guard_loop(LoopBoundary::None), &target).expect("pure guard LICM");
        assert_eq!(stats.versioned_loops, 1);
        assert_eq!(stats.hoisted_instructions, 1);

        // Only the latch's pure owner constant may move; the guard reads the
        // shape the latch writes.
        let (sequence, stats) = optimize(shape_guard_loop(LoopBoundary::LatchShapeWrite), &target)
            .expect("invalidated guard LICM");
        assert_eq!(stats.hoisted_instructions, 1);
        let preheader = &sequence.blocks[1];
        assert!((preheader.first.0..preheader.end.0).all(|index| !matches!(
            sequence.instructions[index as usize].opcode,
            MachineOpcode::CacheIrGuardShape { .. }
        )));
        assert!(sequence.instructions().iter().any(|instruction| matches!(
            instruction.opcode,
            MachineOpcode::CacheIrGuardShape { .. }
        )));
    }

    /// A loop entered from the ordinary entry and from its OSR block: both
    /// external predecessors reach the one preheader that runs the hoisted
    /// guard, and the header parameters they pass stay loop parameters.
    fn osr_entered_shape_guard_loop() -> InstructionSequence {
        let target = TargetSpec::aarch64();
        let receiver = MachineValue(0);
        let active = MachineValue(1);
        let guarded = MachineValue(2);
        let loop_receiver = MachineValue(3);
        let loop_active = MachineValue(4);
        let osr_receiver = MachineValue(5);
        let osr_active = MachineValue(6);
        let mut dispatch = MachineInstruction::plain(
            MachineOpcode::OsrDispatch {
                logical_pcs: vec![1],
            },
            vec![],
        );
        dispatch.control = ControlFlow::Branch;
        let mut jump = MachineInstruction::plain(MachineOpcode::Jump, vec![]);
        jump.control = ControlFlow::Branch;
        let mut instructions = vec![
            dispatch,
            MachineInstruction::plain(
                MachineOpcode::EntryValue(0),
                vec![MachineOperand::register_output(receiver)],
            ),
            MachineInstruction::plain(
                MachineOpcode::BooleanConstant(true),
                vec![MachineOperand::register_output(active)],
            ),
            jump.clone(),
            MachineInstruction::plain(
                MachineOpcode::OsrValue {
                    logical_pc: 1,
                    frame_register: 0,
                    value_type: MachineOsrType::Tagged,
                },
                vec![MachineOperand::register_output(osr_receiver)],
            ),
            MachineInstruction::plain(
                MachineOpcode::OsrValue {
                    logical_pc: 1,
                    frame_register: 1,
                    value_type: MachineOsrType::Boolean,
                },
                vec![MachineOperand::register_output(osr_active)],
            ),
            jump.clone(),
        ];
        let header_start = MachineInstructionId(instructions.len() as u32);
        let mut guard = MachineInstruction::plain(
            MachineOpcode::CacheIrGuardShape {
                byte_pc: 8,
                shape: 1,
            },
            vec![
                MachineOperand::location_input(loop_receiver),
                MachineOperand::register_input(loop_active),
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
        instructions.push(jump);
        let latch_end = MachineInstructionId(instructions.len() as u32);
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(loop_receiver)],
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
                MachineRepresentation::Boolean,
                MachineRepresentation::Tagged,
                MachineRepresentation::Boolean,
            ],
            vec![],
            vec![
                MachineBlockData {
                    first: MachineInstructionId(0),
                    end: MachineInstructionId(1),
                    predecessors: vec![],
                    successors: vec![MachineBlock(1), MachineBlock(2)],
                    parameters: vec![],
                    successor_arguments: vec![vec![], vec![]],
                },
                MachineBlockData {
                    first: MachineInstructionId(1),
                    end: MachineInstructionId(4),
                    predecessors: vec![MachineBlock(0)],
                    successors: vec![MachineBlock(3)],
                    parameters: vec![],
                    successor_arguments: vec![vec![receiver, active]],
                },
                MachineBlockData {
                    first: MachineInstructionId(4),
                    end: header_start,
                    predecessors: vec![MachineBlock(0)],
                    successors: vec![MachineBlock(3)],
                    parameters: vec![],
                    successor_arguments: vec![vec![osr_receiver, osr_active]],
                },
                MachineBlockData {
                    first: header_start,
                    end: header_end,
                    predecessors: vec![MachineBlock(1), MachineBlock(2), MachineBlock(4)],
                    successors: vec![MachineBlock(5), MachineBlock(4)],
                    parameters: vec![loop_receiver, loop_active],
                    successor_arguments: vec![vec![], vec![]],
                },
                MachineBlockData {
                    first: header_end,
                    end: latch_end,
                    predecessors: vec![MachineBlock(3)],
                    successors: vec![MachineBlock(3)],
                    parameters: vec![],
                    successor_arguments: vec![vec![loop_receiver, loop_active]],
                },
                MachineBlockData {
                    first: latch_end,
                    end,
                    predecessors: vec![MachineBlock(3)],
                    successors: vec![],
                    parameters: vec![],
                    successor_arguments: vec![],
                },
            ],
            instructions,
        )
        .expect("OSR-entered guard loop")
    }

    #[test]
    fn osr_block_enters_the_shared_preheader_that_runs_a_hoisted_guard() {
        let target = TargetSpec::aarch64();
        let (sequence, stats) =
            optimize(osr_entered_shape_guard_loop(), &target).expect("OSR-entered guard LICM");
        assert_eq!(stats.versioned_loops, 1);
        assert_eq!(stats.hoisted_instructions, 1);
        let preheader = &sequence.blocks[3];
        assert_eq!(
            preheader.predecessors,
            vec![MachineBlock(1), MachineBlock(2)]
        );
        let opcodes = (preheader.first.0..preheader.end.0)
            .map(|index| &sequence.instructions[index as usize].opcode)
            .collect::<Vec<_>>();
        assert!(matches!(
            opcodes.as_slice(),
            [MachineOpcode::CacheIrGuardShape { .. }, MachineOpcode::Jump]
        ));
    }
}
