//! Effect-aware global value numbering for verified Machine SSA.
//!
//! # Contents
//! - A walk of the shared dominator tree (`super::dominance`) over the
//!   complete normal/exception CFG.
//! - A scoped available-expression table keyed by canonical operands,
//!   representation, dependency epoch, alias class, and memory version, with
//!   a fingerprint index so each lookup is constant time.
//! - Dense instruction/root/exit repair after redundant nodes are removed.
//!
//! # Invariants
//! - A replacement value is defined by a dominating equivalent instruction.
//! - Merge blocks begin a fresh dependency epoch; no path-local heap proof is
//!   guessed through a join.
//! - Alias writes invalidate only overlapping reads. Allocation, throw,
//!   safepoint, and reentry invalidate the whole dependency epoch.
//! - Binding and element-view proofs are reused only inside their own block
//!   and safepoint-free span, so their raw addresses never outlive a collection.
//! - Values carried across CFG edges keep distinct producer identities; GVN
//!   does not extend a leader's live range through block-parameter copies.
//! - Authoritative frame states and inline activation recipes are rewritten
//!   with the same dominating substitutions as executable operands.
//! - GC roots and dense exit ids are rebuilt after instruction removal.
//!
//! # See also
//! - `super::effects` is the sole opcode/effect classification table.

use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHasher};

use super::{
    ControlFlow, DeoptId, InstructionSequence, MachineAliasClass, MachineCommoning, MachineEffects,
    MachineFrameSlot, MachineInstruction, MachineInstructionId, MachineOpcode,
    MachineRepresentation, MachineValue, OperandPurpose, OperandRole, TargetSpec,
    VerificationError, dominance::Dominance, effects::effects_for_instruction,
};

const ALIASES: [MachineAliasClass; MachineAliasClass::COUNT] = [
    MachineAliasClass::Shape,
    MachineAliasClass::PropertyMetadata,
    MachineAliasClass::Prototype,
    MachineAliasClass::PropertyField,
    MachineAliasClass::ElementMetadata,
    MachineAliasClass::ElementField,
    MachineAliasClass::Binding,
    MachineAliasClass::ConstantCell,
    MachineAliasClass::Allocation,
    MachineAliasClass::GcBarrier,
];

/// Deterministic result counters published in optimized artifacts and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MachineOptimizationStats {
    pub(crate) eliminated_instructions: u32,
    pub(crate) eliminated_guards: u32,
    pub(crate) eliminated_loads: u32,
    pub(crate) hoisted_instructions: u32,
    pub(crate) versioned_loops: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpressionKey {
    opcode: MachineOpcode,
    inputs: Vec<MachineValue>,
    outputs: Vec<MachineRepresentation>,
    dependency_epoch: u64,
    memory_versions: Vec<(MachineAliasClass, u64)>,
    /// The block a block-local expression may be reused in.
    block: Option<usize>,
}

#[derive(Debug, Clone)]
struct AvailableExpression {
    key: ExpressionKey,
    outputs: Vec<MachineValue>,
}

/// Dependency epoch and per-alias memory versions in effect at one program
/// point. Every expression key embeds the parts it depends on.
#[derive(Clone, Copy)]
struct ScopeState {
    dependency_epoch: u64,
    memory_versions: [u64; MachineAliasClass::COUNT],
}

impl ScopeState {
    fn entry() -> Self {
        Self {
            dependency_epoch: 0,
            memory_versions: [0; MachineAliasClass::COUNT],
        }
    }

    /// Starts the unique epoch of merge block `block`. No key recorded under
    /// another epoch can equal a key formed after this point, so dominating
    /// entries stay in the table but can no longer be matched.
    fn fresh_merge(&mut self, block: usize) {
        let epoch = ((block as u64) + 1) << 32;
        self.dependency_epoch = epoch;
        self.memory_versions.fill(epoch);
    }

    fn memory_key(&self, effects: MachineEffects) -> Vec<(MachineAliasClass, u64)> {
        ALIASES
            .into_iter()
            .filter(|&alias| effects.reads.contains(alias))
            .map(|alias| (alias, self.memory_versions[alias as usize]))
            .collect()
    }

    fn apply_effects(&mut self, effects: MachineEffects) {
        for alias in ALIASES {
            if effects.writes.contains(alias) {
                self.memory_versions[alias as usize] =
                    self.memory_versions[alias as usize].wrapping_add(1);
            }
        }
        if effects.invalidates_dependency_epoch() {
            self.dependency_epoch = self.dependency_epoch.wrapping_add(1);
            for version in &mut self.memory_versions {
                *version = version.wrapping_add(1);
            }
        }
    }
}

/// Expressions available along the current dominator-tree path.
///
/// A stack of entries plus a fingerprint index: entering a block pushes, and
/// leaving its subtree truncates back to the length saved on entry, so every
/// visible entry was produced by a dominating instruction.
#[derive(Default)]
struct AvailableTable {
    entries: Vec<(u64, AvailableExpression)>,
    index: FxHashMap<u64, Vec<usize>>,
}

impl AvailableTable {
    fn fingerprint(key: &ExpressionKey) -> u64 {
        let mut hasher = FxHasher::default();
        std::mem::discriminant(&key.opcode).hash(&mut hasher);
        for input in &key.inputs {
            input.0.hash(&mut hasher);
        }
        key.dependency_epoch.hash(&mut hasher);
        for (alias, version) in &key.memory_versions {
            (*alias as usize).hash(&mut hasher);
            version.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Most recently recorded dominating entry equal to `key`.
    fn find(&self, key: &ExpressionKey) -> Option<&AvailableExpression> {
        self.index
            .get(&Self::fingerprint(key))?
            .iter()
            .rev()
            .map(|&entry| &self.entries[entry].1)
            .find(|available| available.key == *key)
    }

    fn push(&mut self, expression: AvailableExpression) {
        let fingerprint = Self::fingerprint(&expression.key);
        self.index
            .entry(fingerprint)
            .or_default()
            .push(self.entries.len());
        self.entries.push((fingerprint, expression));
    }

    fn truncate(&mut self, len: usize) {
        while self.entries.len() > len {
            let (fingerprint, _) = self.entries.pop().expect("length checked");
            if let Some(bucket) = self.index.get_mut(&fingerprint) {
                bucket.pop();
            }
        }
    }
}

enum Visit {
    Enter(usize, ScopeState),
    Leave(usize),
}

pub(super) fn optimize(
    mut sequence: InstructionSequence,
    target: &TargetSpec,
) -> Result<(InstructionSequence, MachineOptimizationStats), VerificationError> {
    let dominance = Dominance::compute(sequence.blocks.len(), sequence.entry.0 as usize, |block| {
        sequence.blocks[block]
            .successors
            .iter()
            .map(|successor| successor.0 as usize)
    });
    let mut edge_values = vec![false; sequence.representations.len()];
    for value in sequence
        .blocks
        .iter()
        .flat_map(|block| block.successor_arguments.iter().flatten())
    {
        edge_values[value.0 as usize] = true;
    }
    let mut replacements = (0..sequence.representations.len())
        .map(|value| MachineValue(value as u32))
        .collect::<Vec<_>>();
    let mut eliminated = vec![false; sequence.instructions.len()];
    let mut stats = MachineOptimizationStats::default();
    let mut visited = vec![false; sequence.blocks.len()];
    let mut available = AvailableTable::default();

    let mut roots = vec![sequence.entry.0 as usize];
    roots.extend((0..sequence.blocks.len()).filter(|&block| !dominance.is_reachable(block)));
    for root in roots {
        if visited[root] {
            continue;
        }
        let mut stack = vec![Visit::Enter(root, ScopeState::entry())];
        while let Some(visit) = stack.pop() {
            let (block_index, mut state) = match visit {
                Visit::Enter(block, state) => (block, state),
                Visit::Leave(len) => {
                    available.truncate(len);
                    continue;
                }
            };
            if visited[block_index] {
                continue;
            }
            visited[block_index] = true;
            stack.push(Visit::Leave(available.entries.len()));
            let block = &sequence.blocks[block_index];
            let (first, end) = (block.first.0 as usize, block.end.0 as usize);
            if block_index != sequence.entry.0 as usize && block.predecessors.len() != 1 {
                state.fresh_merge(block_index);
            }

            for (instruction_index, eliminated_slot) in
                eliminated.iter_mut().enumerate().take(end).skip(first)
            {
                let instruction = &mut sequence.instructions[instruction_index];
                rewrite_uses(instruction, &replacements);
                let effects =
                    effects_for_instruction(&instruction.opcode, &sequence.call_descriptors);
                let outputs = instruction
                    .operands
                    .iter()
                    .filter(|operand| {
                        operand.role == OperandRole::Definition
                            && operand.purpose == OperandPurpose::Output
                    })
                    .map(|operand| operand.value)
                    .collect::<Vec<_>>();
                let candidate = (!outputs.iter().any(|output| edge_values[output.0 as usize]))
                    .then(|| {
                        expression_key(
                            instruction,
                            &sequence.representations,
                            effects,
                            &state,
                            block_index,
                        )
                    })
                    .flatten();
                if let Some(key) = candidate {
                    if let Some(leader) = available.find(&key)
                        && leader.outputs.len() == outputs.len()
                    {
                        for (&output, &leader_output) in outputs.iter().zip(&leader.outputs) {
                            replacements[output.0 as usize] = resolve(&replacements, leader_output);
                        }
                        *eliminated_slot = true;
                        stats.eliminated_instructions += 1;
                        match effects.commoning {
                            MachineCommoning::Guard => stats.eliminated_guards += 1,
                            MachineCommoning::Value if !effects.reads.is_empty() => {
                                stats.eliminated_loads += 1;
                            }
                            MachineCommoning::Never | MachineCommoning::Value => {}
                        }
                        continue;
                    }
                    available.push(AvailableExpression { key, outputs });
                }
                state.apply_effects(effects);
                // Every GC safepoint ends the span in which raw addresses stay
                // valid, whatever the opcode's effect row declares.
                if sequence.instructions[instruction_index].safepoint.is_some()
                    && !effects.invalidates_dependency_epoch()
                {
                    state.dependency_epoch = state.dependency_epoch.wrapping_add(1);
                }
            }

            for &child in dominance.children(block_index).iter().rev() {
                stack.push(Visit::Enter(child, state));
            }
        }
    }

    rebuild(&mut sequence, &eliminated, &replacements);
    sequence.verify_pass(target)?;
    Ok((sequence, stats))
}

fn expression_key(
    instruction: &MachineInstruction,
    representations: &[MachineRepresentation],
    effects: MachineEffects,
    state: &ScopeState,
    block_index: usize,
) -> Option<ExpressionKey> {
    if effects.commoning == MachineCommoning::Never
        || effects.allocates
        || effects.throws
        || effects.safepoint
        || effects.reentrant
        || !effects.writes.is_empty()
        || instruction.control != ControlFlow::None
        || instruction.safepoint.is_some()
    {
        return None;
    }
    let outputs = instruction
        .operands
        .iter()
        .filter(|operand| {
            operand.role == OperandRole::Definition && operand.purpose == OperandPurpose::Output
        })
        .map(|operand| representations[operand.value.0 as usize])
        .collect::<Vec<_>>();
    if outputs.is_empty() && effects.commoning != MachineCommoning::Guard {
        return None;
    }
    let inputs = instruction
        .operands
        .iter()
        .filter(|operand| {
            operand.role == OperandRole::Use && operand.purpose == OperandPurpose::Input
        })
        .map(|operand| operand.value)
        .collect();
    Some(ExpressionKey {
        opcode: canonical_opcode(&instruction.opcode),
        inputs,
        outputs,
        dependency_epoch: state.dependency_epoch,
        memory_versions: state.memory_key(effects),
        // A binding or element-view proof's raw addresses stay inside its own
        // block and safepoint-free span (the dependency epoch covers the
        // latter), so a dominating guard in another block never replaces it.
        block: matches!(
            instruction.opcode,
            MachineOpcode::BindingGuard { .. } | MachineOpcode::ElementView { .. }
        )
        .then_some(block_index),
    })
}

fn canonical_opcode(opcode: &MachineOpcode) -> MachineOpcode {
    match opcode {
        MachineOpcode::ElementView { access, .. } => MachineOpcode::ElementView {
            byte_pc: 0,
            access: *access,
        },
        MachineOpcode::ElementAddress { access, .. } => MachineOpcode::ElementAddress {
            byte_pc: 0,
            access: *access,
        },
        MachineOpcode::ElementValueLoad { access, .. } => MachineOpcode::ElementValueLoad {
            byte_pc: 0,
            access: *access,
        },
        MachineOpcode::ElementValueGuard { access, .. } => MachineOpcode::ElementValueGuard {
            byte_pc: 0,
            access: *access,
        },
        MachineOpcode::ElementValueStore { access, .. } => MachineOpcode::ElementValueStore {
            byte_pc: 0,
            access: *access,
        },
        MachineOpcode::NativeLeafIdentity {
            builtin_native_ref, ..
        } => MachineOpcode::NativeLeafIdentity {
            byte_pc: 0,
            builtin_native_ref: *builtin_native_ref,
        },
        MachineOpcode::NativeInt32Math { stub, .. } => MachineOpcode::NativeInt32Math {
            byte_pc: 0,
            stub: *stub,
        },
        MachineOpcode::NativeLeafProbe { stub, .. } => MachineOpcode::NativeLeafProbe {
            byte_pc: 0,
            stub: *stub,
        },
        MachineOpcode::InstanceofProbe { .. } => MachineOpcode::InstanceofProbe { byte_pc: 0 },
        MachineOpcode::LooseEqualityProbe { equal, .. } => MachineOpcode::LooseEqualityProbe {
            byte_pc: 0,
            equal: *equal,
        },
        MachineOpcode::BinaryNumberProbe { operator, .. } => MachineOpcode::BinaryNumberProbe {
            byte_pc: 0,
            operator: *operator,
        },
        MachineOpcode::TaggedNullishEqual { equal, .. } => MachineOpcode::TaggedNullishEqual {
            byte_pc: 0,
            equal: *equal,
        },
        MachineOpcode::BindingGuard {
            semantics, target, ..
        } => MachineOpcode::BindingGuard {
            byte_pc: 0,
            semantics: *semantics,
            target: *target,
        },
        MachineOpcode::BindingHit {
            semantics, target, ..
        } => MachineOpcode::BindingHit {
            byte_pc: 0,
            semantics: *semantics,
            target: *target,
        },
        MachineOpcode::BindingJoin { semantics, .. } => MachineOpcode::BindingJoin {
            byte_pc: 0,
            semantics: *semantics,
        },
        MachineOpcode::StringConstantCellLoad { target, .. } => {
            MachineOpcode::StringConstantCellLoad {
                byte_pc: 0,
                target: *target,
            }
        }
        MachineOpcode::CacheIrGuardShape { shape, .. } => MachineOpcode::CacheIrGuardShape {
            byte_pc: 0,
            shape: *shape,
        },
        MachineOpcode::CacheIrGuardDictionaryLayout { layout, .. } => {
            MachineOpcode::CacheIrGuardDictionaryLayout {
                byte_pc: 0,
                layout: *layout,
            }
        }
        MachineOpcode::CacheIrGuardOrdinaryState { .. } => {
            MachineOpcode::CacheIrGuardOrdinaryState { byte_pc: 0 }
        }
        MachineOpcode::CacheIrGuardAtomSlot {
            atom,
            value_byte,
            writable,
            ..
        } => MachineOpcode::CacheIrGuardAtomSlot {
            byte_pc: 0,
            atom: *atom,
            value_byte: *value_byte,
            writable: *writable,
        },
        MachineOpcode::CacheIrLoadPrototype { .. } => {
            MachineOpcode::CacheIrLoadPrototype { byte_pc: 0 }
        }
        MachineOpcode::CacheIrGuardPrototypeNull { .. } => {
            MachineOpcode::CacheIrGuardPrototypeNull { byte_pc: 0 }
        }
        MachineOpcode::CacheIrGuardArrayIndexProtector { .. } => {
            MachineOpcode::CacheIrGuardArrayIndexProtector { byte_pc: 0 }
        }
        MachineOpcode::CacheIrLoadField { value_byte, .. } => MachineOpcode::CacheIrLoadField {
            byte_pc: 0,
            value_byte: *value_byte,
        },
        MachineOpcode::PropertyShapeProof {
            shape, ordinary, ..
        } => MachineOpcode::PropertyShapeProof {
            byte_pc: 0,
            shape: *shape,
            ordinary: *ordinary,
        },
        MachineOpcode::PropertySlotLoad { value_byte, .. } => MachineOpcode::PropertySlotLoad {
            byte_pc: 0,
            value_byte: *value_byte,
        },
        MachineOpcode::PropertyPolymorphicLoad { cases, .. } => {
            MachineOpcode::PropertyPolymorphicLoad {
                byte_pc: 0,
                cases: cases.clone(),
            }
        }
        MachineOpcode::PropertyMegamorphicLoad { atom, .. } => {
            MachineOpcode::PropertyMegamorphicLoad {
                byte_pc: 0,
                atom: *atom,
            }
        }
        MachineOpcode::CacheIrGuardExtensible { value_byte, .. } => {
            MachineOpcode::CacheIrGuardExtensible {
                byte_pc: 0,
                value_byte: *value_byte,
            }
        }
        MachineOpcode::ExoticLength { .. } => MachineOpcode::ExoticLength { byte_pc: 0 },
        MachineOpcode::CacheIrJoin { store, .. } => MachineOpcode::CacheIrJoin {
            byte_pc: 0,
            store: *store,
        },
        _ => opcode.clone(),
    }
}

/// Redirect every use to its representative value.
///
/// Frame-state and tagged-root operands are sets of values an exit or a
/// safepoint keeps alive, not positional arguments: once two members resolve
/// to the same representative, the duplicate is dropped so the set stays
/// exact.
fn rewrite_uses(instruction: &mut MachineInstruction, replacements: &[MachineValue]) {
    let mut metadata_changed = false;
    for operand in &mut instruction.operands {
        if operand.role == OperandRole::Use {
            let value = resolve(replacements, operand.value);
            metadata_changed |= value != operand.value
                && matches!(
                    operand.purpose,
                    OperandPurpose::FrameState | OperandPurpose::TaggedRoot
                );
            operand.value = value;
        }
    }
    // Metadata operands are already distinct; only a substitution can make
    // two of them name the same value.
    if !metadata_changed {
        return;
    }
    let mut seen = rustc_hash::FxHashSet::default();
    instruction.operands.retain(|operand| {
        !matches!(
            operand.purpose,
            OperandPurpose::FrameState | OperandPurpose::TaggedRoot
        ) || seen.insert((operand.purpose == OperandPurpose::TaggedRoot, operand.value))
    });
}

fn resolve(replacements: &[MachineValue], mut value: MachineValue) -> MachineValue {
    loop {
        let next = replacements[value.0 as usize];
        if next == value {
            return value;
        }
        value = next;
    }
}

fn rebuild(sequence: &mut InstructionSequence, eliminated: &[bool], replacements: &[MachineValue]) {
    for block in &mut sequence.blocks {
        for arguments in &mut block.successor_arguments {
            for argument in arguments {
                *argument = resolve(replacements, *argument);
            }
        }
    }

    for state in &mut sequence.frame_states {
        for object in &mut state.virtual_objects {
            for slot in &mut object.fields {
                if let MachineFrameSlot::Value(value) = slot {
                    *value = resolve(replacements, *value);
                }
            }
        }
        for frame in &mut state.frames {
            for slot in frame
                .entry
                .iter_mut()
                .flat_map(|entry| [&mut entry.new_target, &mut entry.this, &mut entry.closure])
                .chain(frame.slots.iter_mut())
            {
                if let MachineFrameSlot::Value(value) = slot {
                    *value = resolve(replacements, *value);
                }
            }
        }
    }

    let capacity =
        sequence.instructions.len() - eliminated.iter().filter(|&&removed| removed).count();
    let mut old_instructions = std::mem::take(&mut sequence.instructions)
        .into_iter()
        .zip(eliminated.iter().copied());
    let mut instructions = Vec::with_capacity(capacity);
    for block in &mut sequence.blocks {
        // Verified blocks own contiguous ranges in block order, so the
        // instructions move out without a copy.
        let len = (block.end.0 - block.first.0) as usize;
        block.first = MachineInstructionId(instructions.len() as u32);
        for (mut instruction, eliminated) in old_instructions.by_ref().take(len) {
            if eliminated {
                continue;
            }
            rewrite_uses(&mut instruction, replacements);
            for frame in &mut instruction.inline_frames {
                for value in frame
                    .entry
                    .iter_mut()
                    .flat_map(|entry| [&mut entry.new_target, &mut entry.this, &mut entry.closure])
                    .chain(frame.slots.iter_mut())
                    .flatten()
                {
                    *value = resolve(replacements, *value);
                }
            }
            instruction
                .operands
                .retain(|operand| operand.purpose != OperandPurpose::TaggedRoot);
            instructions.push(instruction);
        }
        block.end = MachineInstructionId(instructions.len() as u32);
    }

    let mut next_exit = 0u32;
    for instruction in &mut instructions {
        for exit in &mut instruction.exits {
            exit.id = DeoptId(next_exit);
            next_exit += 1;
        }
    }
    sequence.instructions = instructions;
    sequence.complete_gc_root_liveness();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{MachineBlock, MachineBlockData, MachineOperand, TargetClobberSet};

    fn instruction_count(
        sequence: &InstructionSequence,
        needle: fn(&MachineOpcode) -> bool,
    ) -> usize {
        sequence
            .instructions()
            .iter()
            .filter(|instruction| needle(&instruction.opcode))
            .count()
    }

    fn redundant_shape_sequence(with_shape_write: bool) -> InstructionSequence {
        let target = TargetSpec::aarch64();
        let receiver = MachineValue(0);
        let first_true = MachineValue(1);
        let first_guard = MachineValue(2);
        let owner = MachineValue(3);
        let second_true = MachineValue(4);
        let second_guard = MachineValue(5);
        let mut instructions = vec![
            MachineInstruction::plain(
                MachineOpcode::EntryValue(0),
                vec![MachineOperand::register_output(receiver)],
            ),
            MachineInstruction::plain(
                MachineOpcode::BooleanConstant(true),
                vec![MachineOperand::register_output(first_true)],
            ),
        ];
        {
            let (active, output) = (first_true, first_guard);
            let mut guard = MachineInstruction::plain(
                MachineOpcode::CacheIrGuardShape {
                    byte_pc: 10,
                    shape: 7,
                },
                vec![
                    MachineOperand::location_input(receiver),
                    MachineOperand::register_input(active),
                    MachineOperand::register_output(output),
                ],
            );
            guard.clobbers = target.clobbers(TargetClobberSet::PropertyLoad).to_vec();
            instructions.push(guard);
        }
        instructions.push(MachineInstruction::plain(
            MachineOpcode::IntegerConstant(0),
            vec![MachineOperand::register_output(owner)],
        ));
        if with_shape_write {
            let mut publish = MachineInstruction::plain(
                MachineOpcode::CacheIrPublishShape {
                    byte_pc: 11,
                    shape: 8,
                    new_len: 1,
                    initialize_inline: true,
                },
                vec![
                    MachineOperand::location_input(owner),
                    MachineOperand::register_input(first_guard),
                ],
            );
            publish.clobbers = target.clobbers(TargetClobberSet::PropertyStore).to_vec();
            instructions.push(publish);
        }
        instructions.push(MachineInstruction::plain(
            MachineOpcode::BooleanConstant(true),
            vec![MachineOperand::register_output(second_true)],
        ));
        let mut second = MachineInstruction::plain(
            MachineOpcode::CacheIrGuardShape {
                byte_pc: 99,
                shape: 7,
            },
            vec![
                MachineOperand::location_input(receiver),
                MachineOperand::register_input(second_true),
                MachineOperand::register_output(second_guard),
            ],
        );
        second.clobbers = target.clobbers(TargetClobberSet::PropertyLoad).to_vec();
        instructions.push(second);
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(receiver)],
        );
        ret.control = ControlFlow::Return;
        instructions.push(ret);

        InstructionSequence::new(
            &target,
            MachineBlock(0),
            vec![
                MachineRepresentation::Tagged,
                MachineRepresentation::Boolean,
                MachineRepresentation::Boolean,
                MachineRepresentation::Int64,
                MachineRepresentation::Boolean,
                MachineRepresentation::Boolean,
            ],
            vec![],
            vec![MachineBlockData {
                first: MachineInstructionId(0),
                end: MachineInstructionId(instructions.len() as u32),
                predecessors: vec![],
                successors: vec![],
                parameters: vec![],
                successor_arguments: vec![],
            }],
            instructions,
        )
        .expect("valid redundant-shape fixture")
    }

    #[test]
    fn eliminates_dominating_equivalent_shape_guard_and_constant() {
        let sequence = redundant_shape_sequence(false);
        let (optimized, stats) = optimize(sequence, &TargetSpec::aarch64()).expect("GVN");
        assert_eq!(stats.eliminated_instructions, 2);
        assert_eq!(stats.eliminated_guards, 1);
        assert_eq!(
            instruction_count(&optimized, |opcode| matches!(
                opcode,
                MachineOpcode::CacheIrGuardShape { .. }
            )),
            1
        );
    }

    #[test]
    fn shape_publication_invalidates_a_dominating_shape_guard() {
        let sequence = redundant_shape_sequence(true);
        let (optimized, stats) = optimize(sequence, &TargetSpec::aarch64()).expect("GVN");
        assert_eq!(stats.eliminated_guards, 0);
        assert_eq!(
            instruction_count(&optimized, |opcode| matches!(
                opcode,
                MachineOpcode::CacheIrGuardShape { .. }
            )),
            2
        );
    }

    #[test]
    fn keeps_distinct_producers_for_cfg_edge_values() {
        let target = TargetSpec::aarch64();
        let first = MachineValue(0);
        let second = MachineValue(1);
        let parameter = MachineValue(2);
        let boxed = MachineValue(3);
        let mut jump = MachineInstruction::plain(MachineOpcode::Jump, vec![]);
        jump.control = ControlFlow::Branch;
        let mut ret = MachineInstruction::plain(
            MachineOpcode::Return,
            vec![MachineOperand::register_input(boxed)],
        );
        ret.control = ControlFlow::Return;
        let sequence = InstructionSequence::new(
            &target,
            MachineBlock(0),
            vec![
                MachineRepresentation::Int32,
                MachineRepresentation::Int32,
                MachineRepresentation::Int32,
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
                    successor_arguments: vec![vec![second]],
                },
                MachineBlockData {
                    first: MachineInstructionId(3),
                    end: MachineInstructionId(5),
                    predecessors: vec![MachineBlock(0)],
                    successors: vec![],
                    parameters: vec![parameter],
                    successor_arguments: vec![],
                },
            ],
            vec![
                MachineInstruction::plain(
                    MachineOpcode::IntegerConstant(0),
                    vec![MachineOperand::register_output(first)],
                ),
                MachineInstruction::plain(
                    MachineOpcode::IntegerConstant(0),
                    vec![MachineOperand::register_output(second)],
                ),
                jump,
                MachineInstruction::plain(
                    MachineOpcode::BoxInt32,
                    vec![
                        MachineOperand::register_input(parameter),
                        MachineOperand::register_output(boxed),
                    ],
                ),
                ret,
            ],
        )
        .expect("valid edge-value fixture");

        let (optimized, stats) = optimize(sequence, &target).expect("GVN");
        assert_eq!(stats.eliminated_instructions, 0);
        assert_eq!(
            instruction_count(&optimized, |opcode| matches!(
                opcode,
                MachineOpcode::IntegerConstant(0)
            )),
            2
        );
    }
}
