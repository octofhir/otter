//! Truncated-use selection of wrapping int32 additions, and the int32 values
//! whose uses cannot tell `-0` from `0`.
//!
//! # Contents
//! - [`wrap_truncated_arithmetic`] — turn a checked `Int32Add`/`Int32Sub`
//!   whose every use truncates its result into the two's-complement form.
//! - [`zero_insensitive`] — the int32 values every use of which identifies
//!   `-0` with `0` (TurboFan's `IdentifyZeros` truncation).
//!
//! # Invariants
//! - Both operands are int32, so the exact sum or difference is an integer
//!   below 2^33 in magnitude and is exact as a double. `ToInt32` and
//!   `ToUint32` are additive modulo 2^32, so the wrapped word is what every
//!   truncating use computes from the exact result, also through a chain of
//!   wrapping additions (TurboFan's word32 truncation of `NumberAdd`).
//! - A use truncates when it is a bitwise or shift operand, or a wrapping
//!   addition itself selected by this pass. Comparisons, phis, boxes,
//!   conversions, element indices and stores observe the exact value.
//! - No frame state may record a wrapped value: a deopt resumes the
//!   interpreter with exact values, so a node any frame state captures keeps
//!   its overflow check.
//! - A selected node no longer deopts; it drops its eager frame state.
//! - A use identifies zeros when it applies `ToInt32`/`ToUint32`, compares
//!   int32 operands, reads an element index, or adds the value to an operand
//!   that is never `-0`, or when it is an int32 addition, subtraction,
//!   multiplication, negation or phi whose own uses all identify zeros: such
//!   an operation maps operands that differ only in the sign of a zero to
//!   results that differ only in the sign of a zero.
//!   A frame state is not a use: the interpreter it resumes reads the value
//!   only through the bytecode reads the graph's uses stand for.
//!
//! # See also
//! - [`super::phi_repr`] — unboxes the phis these additions feed.
//! - [`super::builder`] — builds the checked forms from int32 feedback.

use rustc_hash::FxHashSet;

use super::ir::{BlockId, BranchKind, Graph, Kind, NodeId, Repr};

/// Whether `user` reads its input `index` through `ToInt32`/`ToUint32`.
fn truncates(kind: &Kind, wrapping: bool) -> bool {
    match kind {
        Kind::Int32BitAnd
        | Kind::Int32BitOr
        | Kind::Int32BitXor
        | Kind::Int32BitNot
        | Kind::Int32ShiftLeft
        | Kind::Int32ShiftRight
        | Kind::Int32ShiftRightLogical
        | Kind::Uint32ShiftRightToFloat64 => true,
        Kind::Int32Add | Kind::Int32Sub => wrapping,
        _ => false,
    }
}

/// Whether `node` is an int32 whose exact value is never `-0`: a constant or
/// a bitwise result, or a sum or difference that cannot produce one
/// (`x + y` is `-0` only when both are, `x - y` only when `x` is).
fn never_minus_zero(graph: &Graph, node: NodeId, depth: u8) -> bool {
    let data = graph.node(node);
    match &data.kind {
        Kind::ConstInt32(_) => true,
        kind if truncates(kind, false) => true,
        Kind::Int32Add | Kind::Int32AddWrapping if depth > 0 => {
            never_minus_zero(graph, data.inputs[0], depth - 1)
                || never_minus_zero(graph, data.inputs[1], depth - 1)
        }
        Kind::Int32Sub | Kind::Int32SubWrapping if depth > 0 => {
            never_minus_zero(graph, data.inputs[0], depth - 1)
        }
        _ => false,
    }
}

/// Whether `user`, reading its input `index`, sees `-0` and `0` alike by
/// itself (`Some(true)`), only when its own result does (`None`), or tells
/// them apart (`Some(false)`).
fn identifies_zeros(graph: &Graph, user: NodeId, index: usize) -> Option<bool> {
    let data = graph.node(user);
    let other = |position: usize| never_minus_zero(graph, data.inputs[position], 4);
    match &data.kind {
        kind if truncates(kind, false) => Some(true),
        Kind::Int32Compare(_)
        | Kind::Branch {
            kind: BranchKind::Int32(_),
            ..
        } => Some(true),
        Kind::CheckBounds => Some(index == 0),
        Kind::LoadElement(_)
        | Kind::LoadHoleyFloat64Element(_)
        | Kind::LoadElementUint32ToFloat64
        | Kind::CheckElementPresent
        | Kind::CheckHoleyElementPresent(_)
        | Kind::StoreElement(_)
        | Kind::ElementWriteBarrier => Some(index == 1),
        // `x + y` with `y` never `-0` is `y`'s value or `x + y` alike.
        Kind::Int32Add | Kind::Int32AddWrapping if other(1 - index) => Some(true),
        // `x - (-0)` and `x - 0` differ only for an `x` that is `-0`.
        Kind::Int32Sub | Kind::Int32SubWrapping if index == 1 && other(0) => Some(true),
        Kind::Int32Add
        | Kind::Int32Sub
        | Kind::Int32AddWrapping
        | Kind::Int32SubWrapping
        | Kind::Int32Mul
        | Kind::Int32MulExact
        | Kind::Int32MulIdentifyZeros
        | Kind::Int32Negate
        | Kind::Phi => None,
        _ => Some(false),
    }
}

/// The int32 nodes whose every use identifies `-0` with `0`.
pub(crate) fn zero_insensitive(graph: &Graph, layout: &[BlockId]) -> FxHashSet<NodeId> {
    let mut insensitive: FxHashSet<NodeId> = FxHashSet::default();
    let mut uses: Vec<(NodeId, NodeId, usize)> = Vec::new();
    for &block in layout {
        let data = graph.block(block);
        for &node in data
            .phis
            .iter()
            .chain(&data.body)
            .chain(data.control.iter())
        {
            let node_data = graph.node(node);
            if node_data.repr == Repr::Int32 {
                insensitive.insert(node);
            }
            for (index, &input) in node_data.inputs.iter().enumerate() {
                uses.push((input, node, index));
            }
        }
    }
    // Drop values with a use that may tell the zeros apart until the set is
    // closed under its own transparent uses.
    loop {
        let mut dropped = false;
        for &(value, user, index) in &uses {
            if !insensitive.contains(&value) {
                continue;
            }
            let identifies = identifies_zeros(graph, user, index)
                .unwrap_or_else(|| insensitive.contains(&user));
            if !identifies {
                insensitive.remove(&value);
                dropped = true;
            }
        }
        if !dropped {
            break;
        }
    }
    insensitive
}

/// Select every checked int32 addition whose uses all truncate it.
pub(crate) fn wrap_truncated_arithmetic(graph: &mut Graph, layout: &[BlockId]) {
    let mut observed: FxHashSet<NodeId> = FxHashSet::default();
    for state in &mut graph.frame_states {
        state.for_each_value_mut(|value| {
            observed.insert(*value);
        });
    }
    let mut candidates: FxHashSet<NodeId> = FxHashSet::default();
    let mut uses: Vec<(NodeId, NodeId)> = Vec::new();
    for &block in layout {
        let data = graph.block(block);
        for &node in data
            .phis
            .iter()
            .chain(&data.body)
            .chain(data.control.iter())
        {
            let node_data = graph.node(node);
            if matches!(node_data.kind, Kind::Int32Add | Kind::Int32Sub)
                && !observed.contains(&node)
            {
                candidates.insert(node);
            }
            for &input in &node_data.inputs {
                uses.push((input, node));
            }
        }
    }
    if candidates.is_empty() {
        return;
    }
    // Drop candidates with an exact use until the selection is closed under
    // its own wrapping uses.
    loop {
        let mut dropped = false;
        for &(value, user) in &uses {
            if !candidates.contains(&value) {
                continue;
            }
            let wrapping = candidates.contains(&user);
            if !truncates(&graph.node(user).kind, wrapping) {
                candidates.remove(&value);
                dropped = true;
            }
        }
        if !dropped {
            break;
        }
    }
    for node in candidates {
        let data = graph.node_mut(node);
        data.kind = match data.kind {
            Kind::Int32Add => Kind::Int32AddWrapping,
            _ => Kind::Int32SubWrapping,
        };
        data.eager = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append(graph: &mut Graph, block: BlockId, kind: Kind, inputs: &[NodeId], repr: Repr) -> NodeId {
        let node = graph.add_node(kind, inputs, repr);
        graph.node_mut(node).block = Some(block);
        graph.block_mut(block).body.push(node);
        node
    }

    #[test]
    fn a_product_identifies_zeros_only_through_zero_blind_uses() {
        let mut graph = Graph::default();
        let block = graph.new_block();
        let a = append(&mut graph, block, Kind::InitialRegister(0), &[], Repr::Int32);
        let b = append(&mut graph, block, Kind::InitialRegister(1), &[], Repr::Int32);
        let mask = graph.constant(Kind::ConstInt32(0x3fff));
        // Truncated through an addition: the sign of a zero product is lost.
        let truncated = append(&mut graph, block, Kind::Int32Mul, &[a, b], Repr::Int32);
        let sum = append(&mut graph, block, Kind::Int32Add, &[truncated, b], Repr::Int32);
        let masked = append(&mut graph, block, Kind::Int32BitAnd, &[sum, mask], Repr::Int32);
        // Boxed: `-0` is observable.
        let boxed = append(&mut graph, block, Kind::Int32Mul, &[a, b], Repr::Int32);
        let tagged = append(&mut graph, block, Kind::Int32ToTagged, &[boxed], Repr::Tagged);
        // Added into a value that is boxed: the sum keeps the zero's sign.
        let summed = append(&mut graph, block, Kind::Int32Mul, &[a, b], Repr::Int32);
        let exact_sum = append(&mut graph, block, Kind::Int32Add, &[summed, b], Repr::Int32);
        let exact = append(&mut graph, block, Kind::Int32ToTagged, &[exact_sum], Repr::Tagged);
        let returned = graph.add_node(Kind::Return, &[tagged], Repr::None);
        graph.node_mut(returned).block = Some(block);
        graph.block_mut(block).control = Some(returned);
        let _ = (masked, exact);

        // Added to a shift result, which is never `-0`, then boxed: the sum
        // is `-0` only when both operands are.
        let carried = append(&mut graph, block, Kind::Int32Mul, &[a, b], Repr::Int32);
        let shifted = append(&mut graph, block, Kind::Int32ShiftRight, &[a, mask], Repr::Int32);
        let carry = append(&mut graph, block, Kind::Int32Add, &[shifted, carried], Repr::Int32);
        let boxed_carry = append(&mut graph, block, Kind::Int32ToTagged, &[carry], Repr::Tagged);
        let _ = boxed_carry;

        let insensitive = zero_insensitive(&graph, &[block]);
        assert!(insensitive.contains(&carried));
        assert!(!insensitive.contains(&carry));
        assert!(insensitive.contains(&truncated));
        assert!(insensitive.contains(&sum));
        assert!(!insensitive.contains(&boxed));
        assert!(!insensitive.contains(&summed));
        assert!(!insensitive.contains(&exact_sum));
    }
}
