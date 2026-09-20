//! Partial escape analysis for literal allocations in Machine HIR.
//!
//! # Contents
//! - SSA-use classification for plain-object and fixed-array allocations.
//! - Scalar replacement of exact fixed-array element reads and self identity.
//! - Authoritative `FrameState` virtual-object recipes for cold deopt.
//!
//! # Invariants
//! - Unknown uses, CFG phi transport, nested virtual allocations, inline frame
//!   chains, holes, stores, calls, and reentrant effects escape by default.
//! - A selected allocation has no executable object use after rewriting; its
//!   only surviving identity is a virtual reference inside `FrameState`.
//! - Deopt recipes contain scalar SSA fields and use one dense state-local
//!   materialization order. Target selection and emission never elide an
//!   allocation or invent virtual-object state.
//! - A safepoint cannot observe a virtual allocation: any call/allocation use
//!   classifies the object as escaping before this pass rewrites the graph.
//!
//! # See also
//! - Graal `PartialEscapePhase` propagates virtual-object state through SSA
//!   connections and materializes at observable boundaries.
//! - SpiderMonkey `ScalarReplacement.cpp` uses the same unknown-use-is-escape
//!   rule for incrementally supported allocation and load families.

use std::collections::{BTreeMap, BTreeSet};

use otter_vm::deopt::{VirtualObject, VirtualObjectId, VirtualObjectKind};

use super::hir::{
    NumericDirectCallArguments, NumericFrameSlot, NumericFunction, NumericNode, NumericTerminator,
    NumericType, NumericValue,
};

#[derive(Debug, Clone)]
struct Candidate {
    value: NumericValue,
    kind: VirtualObjectKind,
    fields: Vec<NumericValue>,
}

#[derive(Debug, Clone, Copy)]
struct ScalarLoad {
    node: NumericValue,
    field: NumericValue,
}

/// Deterministic optimization counters published with Machine artifacts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct PartialEscapeStats {
    pub(super) virtualized_allocations: u32,
    pub(super) eliminated_allocations: u32,
    pub(super) scalar_replaced_loads: u32,
    pub(super) materialization_recipes: u32,
}

pub(super) fn optimize(function: &mut NumericFunction) -> PartialEscapeStats {
    let candidates = literal_candidates(function);
    if candidates.is_empty() {
        return PartialEscapeStats::default();
    }

    let candidate_values = candidates
        .iter()
        .map(|candidate| candidate.value)
        .collect::<BTreeSet<_>>();
    let mut stats = PartialEscapeStats::default();
    let mut accepted = BTreeMap::new();
    let mut loads = BTreeMap::<NumericValue, Vec<ScalarLoad>>::new();
    let mut identity_nodes = BTreeMap::<NumericValue, Vec<(NumericValue, bool)>>::new();

    for candidate in candidates {
        if candidate
            .fields
            .iter()
            .any(|field| candidate_values.contains(field) || is_hole(function, *field))
        {
            continue;
        }
        if function.operand_values.contains(&candidate.value) {
            continue;
        }
        if cfg_uses(function, candidate.value) {
            continue;
        }
        if !frame_uses_are_virtualizable(function, candidate.value) {
            continue;
        }

        let mut candidate_loads = Vec::new();
        let mut candidate_identity = Vec::new();
        let mut escapes = false;
        for (index, node) in function.nodes.iter().copied().enumerate() {
            let node_value = NumericValue(index);
            if node_value == candidate.value {
                continue;
            }
            match node {
                NumericNode::ElementLoad {
                    receiver,
                    index,
                    exceptional_edge: None,
                    ..
                } if receiver == candidate.value => {
                    let Some(index) = exact_index(function, index) else {
                        escapes = true;
                        break;
                    };
                    let Some(&field) = candidate.fields.get(index) else {
                        escapes = true;
                        break;
                    };
                    candidate_loads.push(ScalarLoad {
                        node: node_value,
                        field,
                    });
                }
                NumericNode::TaggedStrictEqual(left, right)
                    if (left == candidate.value || right == candidate.value)
                        && candidate_values.contains(&left)
                        && candidate_values.contains(&right) =>
                {
                    candidate_identity.push((node_value, left == right));
                }
                _ if node_inputs(function, node).contains(&candidate.value) => {
                    escapes = true;
                    break;
                }
                _ => {}
            }
        }
        if escapes {
            continue;
        }
        if !region_is_virtualizable(function, &candidate, &candidate_loads, &candidate_identity) {
            continue;
        }
        loads.insert(candidate.value, candidate_loads);
        identity_nodes.insert(candidate.value, candidate_identity);
        accepted.insert(candidate.value, candidate);
    }

    if accepted.is_empty() {
        return stats;
    }

    let mut replacements = (0..function.nodes.len())
        .map(NumericValue)
        .collect::<Vec<_>>();
    let mut removed_nodes = BTreeSet::new();
    for (&allocation, candidate) in &accepted {
        removed_nodes.insert(allocation);
        stats.virtualized_allocations = stats.virtualized_allocations.saturating_add(1);
        stats.eliminated_allocations = stats.eliminated_allocations.saturating_add(1);
        for load in &loads[&candidate.value] {
            if function.nodes[load.field.0].value_type() == NumericType::Tagged {
                replacements[load.node.0] = load.field;
                removed_nodes.insert(load.node);
            } else {
                function.nodes[load.node.0] = NumericNode::BoxTagged(load.field);
            }
            stats.scalar_replaced_loads = stats.scalar_replaced_loads.saturating_add(1);
        }
        for &(identity, equal) in &identity_nodes[&candidate.value] {
            function.nodes[identity.0] = NumericNode::BooleanConstant(equal);
        }
    }

    rewrite_function_uses(function, &replacements);
    for block in &mut function.blocks {
        block.nodes.retain(|node| !removed_nodes.contains(node));
    }
    function.frame_states.retain(|state| match state.point {
        super::frame_state::NumericFramePoint::Node(node) => {
            !removed_nodes.contains(&node) && function.nodes[node.0].frame_state_purpose().is_some()
        }
        super::frame_state::NumericFramePoint::Backedge { .. } => true,
    });

    for state in &mut function.frame_states {
        let mut state_candidates = BTreeSet::new();
        for frame in &state.frames {
            for slot in &frame.slots {
                if let NumericFrameSlot::Value(value) = slot
                    && accepted.contains_key(value)
                {
                    state_candidates.insert(*value);
                }
            }
        }
        let ids = state_candidates
            .iter()
            .enumerate()
            .map(|(id, value)| (*value, VirtualObjectId(id as u32)))
            .collect::<BTreeMap<_, _>>();
        for frame in &mut state.frames {
            for slot in &mut frame.slots {
                if let NumericFrameSlot::Value(value) = *slot
                    && let Some(&object) = ids.get(&value)
                {
                    *slot = NumericFrameSlot::VirtualObject(object);
                }
            }
        }
        state.virtual_objects = state_candidates
            .iter()
            .map(|value| {
                let candidate = &accepted[value];
                VirtualObject {
                    id: ids[value],
                    kind: candidate.kind,
                    fields: candidate
                        .fields
                        .iter()
                        .map(|field| NumericFrameSlot::Value(resolve(&replacements, *field)))
                        .collect(),
                }
            })
            .collect();
        stats.materialization_recipes = stats
            .materialization_recipes
            .saturating_add(state.virtual_objects.len() as u32);
    }

    stats
}

fn literal_candidates(function: &NumericFunction) -> Vec<Candidate> {
    function
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(index, node)| {
            let NumericNode::LiteralAllocation {
                target,
                argument_start,
                argument_count,
                ..
            } = *node
            else {
                return None;
            };
            let kind = if target == otter_vm::native_abi::STUB_JIT_NEW_OBJECT {
                VirtualObjectKind::PlainObject
            } else if target == otter_vm::native_abi::STUB_JIT_NEW_ARRAY {
                VirtualObjectKind::FixedArray
            } else {
                return None;
            };
            let end = argument_start.checked_add(argument_count)? as usize;
            let fields = function
                .operand_values
                .get(argument_start as usize..end)?
                .to_vec();
            Some(Candidate {
                value: NumericValue(index),
                kind,
                fields,
            })
        })
        .collect()
}

fn exact_index(function: &NumericFunction, value: NumericValue) -> Option<usize> {
    match function.nodes.get(value.0)? {
        NumericNode::IntegerConstant(index) => usize::try_from(*index).ok(),
        NumericNode::Constant(number) => {
            (*number >= 0.0 && number.fract() == 0.0).then_some(*number as usize)
        }
        NumericNode::TaggedConstant(bits) => {
            let number = otter_vm::Value::from_bits(*bits).as_number()?.as_f64();
            (number >= 0.0 && number.fract() == 0.0).then_some(number as usize)
        }
        _ => None,
    }
}

fn is_hole(function: &NumericFunction, value: NumericValue) -> bool {
    matches!(
        function.nodes.get(value.0),
        Some(NumericNode::TaggedConstant(bits)) if otter_vm::Value::from_bits(*bits).is_hole()
    )
}

fn frame_uses_are_virtualizable(function: &NumericFunction, value: NumericValue) -> bool {
    function.frame_states.iter().all(|state| {
        let used = state.frames.iter().any(|frame| {
            frame.slots.contains(&NumericFrameSlot::Value(value))
                || frame.entry.iter().any(|entry| {
                    [entry.this, entry.closure, entry.new_target]
                        .contains(&NumericFrameSlot::Value(value))
                })
        });
        !used
            || (state.frames.len() == 1
                && state.frames[0].entry.is_none()
                && !state
                    .virtual_objects
                    .iter()
                    .any(|object| object.fields.contains(&NumericFrameSlot::Value(value))))
    })
}

fn region_is_virtualizable(
    function: &NumericFunction,
    candidate: &Candidate,
    loads: &[ScalarLoad],
    identities: &[(NumericValue, bool)],
) -> bool {
    let Some((block_index, allocation_position)) = node_position(function, candidate.value) else {
        return false;
    };
    let Some(dominators) = numeric_dominators(&function.blocks) else {
        return false;
    };
    let mut endpoint_positions = BTreeMap::new();
    for value in loads
        .iter()
        .map(|load| load.node)
        .chain(identities.iter().map(|(node, _)| *node))
    {
        let Some((use_block, position)) = node_position(function, value) else {
            return false;
        };
        if !dominators[use_block].contains(&block_index)
            || (use_block == block_index && position < allocation_position)
        {
            return false;
        }
        endpoint_positions
            .entry(use_block)
            .and_modify(|last: &mut usize| *last = (*last).max(position))
            .or_insert(position);
    }

    for state in &function.frame_states {
        let uses_candidate = state.frames.iter().any(|frame| {
            frame
                .slots
                .contains(&NumericFrameSlot::Value(candidate.value))
                || frame.entry.iter().any(|entry| {
                    [entry.this, entry.closure, entry.new_target]
                        .contains(&NumericFrameSlot::Value(candidate.value))
                })
        });
        if !uses_candidate {
            continue;
        }
        let super::frame_state::NumericFramePoint::Node(point) = state.point else {
            return false;
        };
        let Some((state_block, position)) = node_position(function, point) else {
            return false;
        };
        if !dominators[state_block].contains(&block_index)
            || (state_block == block_index && position < allocation_position)
        {
            return false;
        }
        if function.nodes[point.0].frame_state_purpose()
            == Some(super::frame_state::NumericFrameStatePurpose::TaggedRoots)
            && !loads.iter().any(|load| load.node == point)
        {
            return false;
        }
        endpoint_positions
            .entry(state_block)
            .and_modify(|last: &mut usize| *last = (*last).max(position))
            .or_insert(position);
    }

    if endpoint_positions.is_empty() {
        return true;
    }
    let mut active_blocks = endpoint_positions.keys().copied().collect::<BTreeSet<_>>();
    let mut pending = active_blocks.iter().copied().collect::<Vec<_>>();
    while let Some(active) = pending.pop() {
        if active == block_index {
            continue;
        }
        for &predecessor in &function.blocks[active].predecessors {
            if dominators[predecessor].contains(&block_index) && active_blocks.insert(predecessor) {
                pending.push(predecessor);
            }
        }
    }

    active_blocks.into_iter().all(|active| {
        let block = &function.blocks[active];
        let start = if active == block_index {
            allocation_position + 1
        } else {
            0
        };
        let end = endpoint_positions
            .get(&active)
            .map_or(block.nodes.len(), |position| position + 1);
        block.nodes[start..end].iter().copied().all(|node| {
            loads.iter().any(|load| load.node == node)
                || function.nodes[node.0].frame_state_purpose()
                    != Some(super::frame_state::NumericFrameStatePurpose::TaggedRoots)
        })
    })
}

fn numeric_dominators(blocks: &[super::hir::NumericBlock]) -> Option<Vec<BTreeSet<usize>>> {
    if blocks.is_empty() {
        return None;
    }
    let all = (0..blocks.len()).collect::<BTreeSet<_>>();
    let mut dominators = vec![all; blocks.len()];
    dominators[0] = BTreeSet::from([0]);
    loop {
        let mut changed = false;
        for block_index in 1..blocks.len() {
            let block = blocks.get(block_index)?;
            let mut next = block
                .predecessors
                .iter()
                .map(|&predecessor| dominators.get(predecessor).cloned())
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .reduce(|left, right| left.intersection(&right).copied().collect())?;
            next.insert(block_index);
            if next != dominators[block_index] {
                dominators[block_index] = next;
                changed = true;
            }
        }
        if !changed {
            return Some(dominators);
        }
    }
}

fn node_position(function: &NumericFunction, value: NumericValue) -> Option<(usize, usize)> {
    function
        .blocks
        .iter()
        .enumerate()
        .find_map(|(block, data)| {
            data.nodes
                .iter()
                .position(|node| *node == value)
                .map(|position| (block, position))
        })
}

fn cfg_uses(function: &NumericFunction, value: NumericValue) -> bool {
    function.blocks.iter().any(|block| {
        block
            .successor_arguments
            .iter()
            .flatten()
            .any(|argument| *argument == value)
            || match block.terminator {
                NumericTerminator::Branch { condition, .. }
                | NumericTerminator::Return(condition)
                | NumericTerminator::Throw(condition) => condition == value,
                NumericTerminator::Jump => false,
            }
    })
}

fn span_values(
    function: &NumericFunction,
    start: u32,
    count: u32,
) -> impl Iterator<Item = NumericValue> + '_ {
    let end = start.saturating_add(count) as usize;
    function
        .operand_values
        .get(start as usize..end)
        .unwrap_or_default()
        .iter()
        .copied()
}

fn node_inputs(function: &NumericFunction, node: NumericNode) -> Vec<NumericValue> {
    use NumericNode as N;
    let mut inputs = Vec::new();
    match node {
        N::Parameter { .. }
        | N::BlockParameter(_)
        | N::TaggedConstant(_)
        | N::This
        | N::StringConstantCell { .. }
        | N::ColdCallExit { .. }
        | N::IntegerConstant(_)
        | N::BooleanConstant(_)
        | N::Constant(_) => {}
        N::TaggedToNumber(value)
        | N::TaggedToInt32(value)
        | N::ClassSuperConstructor(value)
        | N::ConstructReceiverHit(value)
        | N::BoxTagged(value)
        | N::ArrayConstruct { length: value, .. }
        | N::TaggedToBoolean(value)
        | N::WidenInt32(value)
        | N::WidenUint32(value)
        | N::FloatToInt32(value)
        | N::BooleanToInt32(value)
        | N::IntegerNeg(value)
        | N::IntegerNot(value)
        | N::IntegerToBoolean(value)
        | N::FloatToBoolean(value)
        | N::BooleanNot(value)
        | N::Neg(value)
        | N::TaggedNullishEqual { value, .. }
        | N::InlineMethodGuard { source: value, .. }
        | N::InlineConstructGuard { source: value, .. }
        | N::InlineCallGuard { source: value, .. }
        | N::ConstructReceiver { source: value, .. } => inputs.push(value),
        N::IntegerAddImmediate(value, _)
        | N::IntegerSubImmediate(value, _)
        | N::IntegerAndImmediate(value, _)
        | N::IntegerLessThanImmediate(value, _)
        | N::IntegerEqualImmediate(value, _)
        | N::IntegerNotEqualImmediate(value, _) => inputs.push(value),
        N::BaseConstructResult { result, receiver } => inputs.extend([result, receiver]),
        N::ConstructorFieldStore { object, value, .. }
        | N::PropertyStore {
            receiver: object,
            value,
            ..
        } => inputs.extend([object, value]),
        N::PropertyLoad { receiver, .. } => inputs.push(receiver),
        N::ElementLoad {
            receiver, index, ..
        } => inputs.extend([receiver, index]),
        N::ElementStore {
            receiver,
            index,
            value,
            ..
        } => inputs.extend([receiver, index, value]),
        N::Binding { inputs: values, .. } | N::CommittedValue { inputs: values, .. } => {
            inputs.extend(values.into_iter().flatten());
        }
        N::LiteralAllocation {
            argument_start,
            argument_count,
            ..
        } => inputs.extend(span_values(function, argument_start, argument_count)),
        N::DirectCall {
            source, arguments, ..
        } => {
            inputs.push(source);
            match arguments {
                NumericDirectCallArguments::Fixed { start, count } => {
                    inputs.extend(span_values(function, start, count));
                }
                NumericDirectCallArguments::Spread(value) => inputs.push(value),
            }
        }
        N::NativeLeaf {
            source,
            target,
            argument_start,
            ..
        } => {
            inputs.push(source);
            inputs.extend(span_values(
                function,
                argument_start,
                u32::from(target.argument_count),
            ));
        }
        N::NativeCall {
            receiver,
            target,
            argument_start,
            ..
        } => {
            inputs.push(receiver);
            if let super::hir::NumericNativeCallTarget::Resolved { callee, .. } = target {
                inputs.push(callee);
            }
            inputs.extend(span_values(
                function,
                argument_start,
                u32::from(target.declaration().argument_count),
            ));
        }
        N::TaggedStrictEqual(left, right)
        | N::TaggedStringConcat(left, right)
        | N::IntegerAdd(left, right)
        | N::IntegerSub(left, right)
        | N::IntegerMul(left, right)
        | N::IntegerAnd(left, right)
        | N::IntegerOr(left, right)
        | N::IntegerXor(left, right)
        | N::IntegerShiftLeft(left, right)
        | N::IntegerShiftRight(left, right)
        | N::IntegerShiftRightLogical(left, right)
        | N::IntegerEqual(left, right)
        | N::IntegerNotEqual(left, right)
        | N::IntegerLessThan(left, right)
        | N::IntegerLessEqual(left, right)
        | N::IntegerGreaterThan(left, right)
        | N::IntegerGreaterEqual(left, right)
        | N::Add(left, right)
        | N::Sub(left, right)
        | N::Mul(left, right)
        | N::Div(left, right)
        | N::Rem(left, right)
        | N::Pow(left, right)
        | N::LessThan(left, right)
        | N::Equal(left, right)
        | N::NotEqual(left, right)
        | N::LessEqual(left, right)
        | N::GreaterThan(left, right)
        | N::GreaterEqual(left, right) => inputs.extend([left, right]),
    }
    inputs
}

fn resolve(replacements: &[NumericValue], mut value: NumericValue) -> NumericValue {
    loop {
        let next = replacements[value.0];
        if next == value {
            return value;
        }
        value = next;
    }
}

fn rewrite_function_uses(function: &mut NumericFunction, replacements: &[NumericValue]) {
    for value in &mut function.operand_values {
        *value = resolve(replacements, *value);
    }
    for block in &mut function.blocks {
        for arguments in &mut block.successor_arguments {
            for value in arguments {
                *value = resolve(replacements, *value);
            }
        }
        match &mut block.terminator {
            NumericTerminator::Branch { condition, .. }
            | NumericTerminator::Return(condition)
            | NumericTerminator::Throw(condition) => {
                *condition = resolve(replacements, *condition);
            }
            NumericTerminator::Jump => {}
        }
    }
    for node in &mut function.nodes {
        rewrite_node(node, replacements);
    }
    for state in &mut function.frame_states {
        for frame in &mut state.frames {
            for slot in frame
                .entry
                .iter_mut()
                .flat_map(|entry| [&mut entry.this, &mut entry.closure, &mut entry.new_target])
                .chain(frame.slots.iter_mut())
            {
                if let NumericFrameSlot::Value(value) = slot {
                    *value = resolve(replacements, *value);
                }
            }
        }
        for object in &mut state.virtual_objects {
            for slot in &mut object.fields {
                if let NumericFrameSlot::Value(value) = slot {
                    *value = resolve(replacements, *value);
                }
            }
        }
    }
}

fn rewrite_node(node: &mut NumericNode, replacements: &[NumericValue]) {
    let replacement = |value: &mut NumericValue| *value = resolve(replacements, *value);
    use NumericNode as N;
    match node {
        N::TaggedToNumber(value)
        | N::TaggedToInt32(value)
        | N::ClassSuperConstructor(value)
        | N::ConstructReceiverHit(value)
        | N::BoxTagged(value)
        | N::TaggedToBoolean(value)
        | N::WidenInt32(value)
        | N::WidenUint32(value)
        | N::FloatToInt32(value)
        | N::BooleanToInt32(value)
        | N::IntegerNeg(value)
        | N::IntegerNot(value)
        | N::IntegerToBoolean(value)
        | N::FloatToBoolean(value)
        | N::BooleanNot(value)
        | N::Neg(value) => replacement(value),
        N::ArrayConstruct { length, .. } => replacement(length),
        N::TaggedNullishEqual { value, .. } => replacement(value),
        N::InlineMethodGuard { source, .. }
        | N::InlineConstructGuard { source, .. }
        | N::InlineCallGuard { source, .. }
        | N::ConstructReceiver { source, .. }
        | N::NativeLeaf { source, .. } => replacement(source),
        N::IntegerAddImmediate(value, _)
        | N::IntegerSubImmediate(value, _)
        | N::IntegerAndImmediate(value, _)
        | N::IntegerLessThanImmediate(value, _)
        | N::IntegerEqualImmediate(value, _)
        | N::IntegerNotEqualImmediate(value, _) => replacement(value),
        N::BaseConstructResult { result, receiver } => {
            replacement(result);
            replacement(receiver);
        }
        N::ConstructorFieldStore { object, value, .. } => {
            replacement(object);
            replacement(value);
        }
        N::PropertyLoad { receiver, .. } => replacement(receiver),
        N::NativeCall {
            receiver, target, ..
        } => {
            replacement(receiver);
            if let super::hir::NumericNativeCallTarget::Resolved { callee, .. } = target {
                replacement(callee);
            }
        }
        N::PropertyStore {
            receiver, value, ..
        } => {
            replacement(receiver);
            replacement(value);
        }
        N::ElementLoad {
            receiver, index, ..
        } => {
            replacement(receiver);
            replacement(index);
        }
        N::ElementStore {
            receiver,
            index,
            value,
            ..
        } => {
            replacement(receiver);
            replacement(index);
            replacement(value);
        }
        N::Binding { inputs, .. } | N::CommittedValue { inputs, .. } => {
            for value in inputs.iter_mut().flatten() {
                replacement(value);
            }
        }
        N::DirectCall {
            source,
            arguments: NumericDirectCallArguments::Spread(spread),
            ..
        } => {
            replacement(source);
            replacement(spread);
        }
        N::DirectCall { source, .. } => replacement(source),
        N::TaggedStrictEqual(left, right)
        | N::TaggedStringConcat(left, right)
        | N::IntegerAdd(left, right)
        | N::IntegerSub(left, right)
        | N::IntegerMul(left, right)
        | N::IntegerAnd(left, right)
        | N::IntegerOr(left, right)
        | N::IntegerXor(left, right)
        | N::IntegerShiftLeft(left, right)
        | N::IntegerShiftRight(left, right)
        | N::IntegerShiftRightLogical(left, right)
        | N::IntegerEqual(left, right)
        | N::IntegerNotEqual(left, right)
        | N::IntegerLessThan(left, right)
        | N::IntegerLessEqual(left, right)
        | N::IntegerGreaterThan(left, right)
        | N::IntegerGreaterEqual(left, right)
        | N::Add(left, right)
        | N::Sub(left, right)
        | N::Mul(left, right)
        | N::Div(left, right)
        | N::Rem(left, right)
        | N::Pow(left, right)
        | N::LessThan(left, right)
        | N::Equal(left, right)
        | N::NotEqual(left, right)
        | N::LessEqual(left, right)
        | N::GreaterThan(left, right)
        | N::GreaterEqual(left, right) => {
            replacement(left);
            replacement(right);
        }
        N::Parameter { .. }
        | N::BlockParameter(_)
        | N::TaggedConstant(_)
        | N::This
        | N::StringConstantCell { .. }
        | N::LiteralAllocation { .. }
        | N::ColdCallExit { .. }
        | N::IntegerConstant(_)
        | N::BooleanConstant(_)
        | N::Constant(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::numeric::frame_state::{NumericFramePoint, NumericFrameState};
    use crate::machine::numeric::hir::NumericBlock;
    use otter_vm::deopt::{DeoptFrame, VirtualObjectKind};

    #[test]
    fn virtualizes_literal_and_keeps_deopt_materialization_in_frame_state() {
        let value = NumericValue;
        let mut function = NumericFunction {
            function_id: 1,
            nodes: vec![
                NumericNode::TaggedConstant(otter_vm::Value::number_i32(7).to_bits()),
                NumericNode::LiteralAllocation {
                    target: otter_vm::native_abi::STUB_JIT_NEW_ARRAY,
                    argument_start: 0,
                    argument_count: 1,
                    logical_pc: 1,
                    byte_pc: 8,
                },
                NumericNode::TaggedToInt32(value(0)),
                NumericNode::IntegerAddImmediate(value(2), 1),
            ],
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            blocks: vec![NumericBlock {
                logical_pc: 0,
                osr_entry_allowed: false,
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                parameter_registers: Vec::new(),
                successor_arguments: Vec::new(),
                nodes: (0..4).map(value).collect(),
                terminator: NumericTerminator::Return(value(3)),
            }],
            frame_states: vec![NumericFrameState {
                point: NumericFramePoint::Node(value(2)),
                frames: Box::new([DeoptFrame {
                    function_id: 1,
                    byte_pc: 16,
                    entry: None,
                    slots: Box::new([
                        NumericFrameSlot::Value(value(1)),
                        NumericFrameSlot::Value(value(0)),
                    ]),
                }]),
                virtual_objects: Box::default(),
            }],
            direct_call_targets: Vec::new(),
            operand_values: vec![value(0)],
            parameter_count: 0,
            register_count: 2,
            arithmetic_op_count: 1,
        };

        let stats = optimize(&mut function);

        assert_eq!(stats.virtualized_allocations, 1);
        assert_eq!(stats.eliminated_allocations, 1);
        assert!(!function.blocks[0].nodes.contains(&value(1)));
        assert_eq!(function.frame_states[0].virtual_objects.len(), 1);
        assert_eq!(
            function.frame_states[0].virtual_objects[0].kind,
            VirtualObjectKind::FixedArray
        );
        assert_eq!(
            function.frame_states[0].frames[0].slots[0],
            NumericFrameSlot::VirtualObject(VirtualObjectId(0))
        );
    }
}
