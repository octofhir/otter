//! Diamonds that only pick a value become selects.
//!
//! `cond ? a : b` over values already computed, or over fields of objects
//! proved before the test, is a branch to two arms that each hand one value
//! to a merge. A conditional select picks it with the test's flags: no
//! branch to mispredict, no jump over the other arm, no merge. Inlined
//! accessors make this common: `input()` and `output()` of a constraint each
//! pick one of two fields on the same `direction` test.
//!
//! # Contents
//! - [`convert`] — replace every such diamond of `layout` with selects in
//!   the testing block.
//!
//! # Invariants
//! - A diamond is a block ending in a branch whose two successors differ,
//!   each entered only from that block, each ending in a jump to one merge
//!   block that they alone enter and that heads no loop.
//! - An arm holds at most [`ARM_NODES`] nodes, each a pure operation that
//!   cannot trap or a field load: nothing calls, writes, collects, throws or
//!   deopts, so running an arm on the path that did not need it changes
//!   nothing observable. A field load runs early only when the test cannot
//!   be what proved its object: the test computes from the object neither
//!   directly nor through a comparison, type test or shape word. A test of a
//!   value loaded from the object (`this.direction == 1`) proves nothing
//!   about the object.
//! - Arms are matched after [`super::straighten`] merged each branch edge's
//!   block with the block it alone enters.
//! - Every phi of the merge becomes a select of its two inputs on the
//!   branch's condition, placed in the testing block after both arms'
//!   nodes; the merge keeps no phi and the testing block jumps to it.
//! - Only conditions both backends select on convert (int32 and float64
//!   compares, tagged and word equality, a boolean's truth), and only
//!   merges of tagged, int32 or float64 values.
//!
//! # See also
//! - [`super::fold`] — decides branches before diamonds are converted.
//! - [`super::branch_thread`] — threads branches on merged booleans.

use smallvec::SmallVec;

use super::ir::{BlockId, BranchKind, Graph, Kind, NodeId, Repr};

/// The most nodes one arm may hold and still run on both paths.
pub(crate) const ARM_NODES: usize = 4;

/// How many operands the condition of `kind` reads, when a select can test
/// it.
fn condition_operands(graph: &Graph, kind: BranchKind, inputs: &[NodeId]) -> Option<usize> {
    match kind {
        BranchKind::Int32(_)
        | BranchKind::Float64(_)
        | BranchKind::TaggedEqual
        | BranchKind::WordEqual(_) => Some(kind.operands()),
        BranchKind::Truthy if graph.node(inputs[0]).kind.produces_boolean() => Some(1),
        BranchKind::Truthy
        | BranchKind::Float64Truthy
        | BranchKind::Nullish
        | BranchKind::Native(_) => None,
    }
}

/// Whether `kind` reads a value stored in an object: a field, an element, a
/// length or a cell. Such a value says nothing about the object it was read
/// from, unlike a test of the object itself, its type or its shape word.
fn loads_stored_value(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::LoadOwnField { .. }
            | Kind::LoadNumberOwnField { .. }
            | Kind::LoadTaggedField(_)
            | Kind::LoadShapedProperty { .. }
            | Kind::LoadNamedProperty { .. }
            | Kind::LoadPropertyCached { .. }
            | Kind::LoadKeyedCached { .. }
            | Kind::LoadArrayExpando(_)
            | Kind::LoadFunctionBag
            | Kind::LoadFunctionBagCell(_)
            | Kind::LoadPresentElement
            | Kind::LoadElementOrUndefined
            | Kind::LoadElement(_)
            | Kind::LoadElementUint32ToFloat64
            | Kind::LoadHoleyFloat64Element(_)
            | Kind::LoadCellValue(_)
            | Kind::LoadArrayLength { .. }
            | Kind::LoadTypedArrayLength
            | Kind::LoadStringLength
            | Kind::LoadElementsLength { .. }
    )
}

/// Whether testing `node` may prove something about `object`: `node` is
/// `object`, or computes from it (a comparison, a type test, a shape word)
/// at most `depth` levels down without passing through a value stored in an
/// object.
fn tests_object(graph: &Graph, node: NodeId, object: NodeId, depth: u32) -> bool {
    if node == object {
        return true;
    }
    let data = graph.node(node);
    depth > 0
        && !loads_stored_value(&data.kind)
        && data
            .inputs
            .iter()
            .any(|&input| tests_object(graph, input, object, depth - 1))
}

/// Whether `node` may run before the test `condition` instead of below one
/// of its edges.
fn runs_early(graph: &Graph, node: NodeId, condition: &[NodeId]) -> bool {
    let data = graph.node(node);
    match data.kind {
        Kind::Int32ToTagged
        | Kind::Float64ToTagged
        | Kind::Int32ToFloat64
        | Kind::Int32AddWrapping
        | Kind::Int32SubWrapping
        | Kind::Int32BitAnd
        | Kind::Int32BitOr
        | Kind::Int32BitXor
        | Kind::Int32ShiftLeft
        | Kind::Int32ShiftRight
        | Kind::Float64Add
        | Kind::Float64Sub
        | Kind::Float64Mul
        | Kind::Float64Div
        | Kind::Float64Negate => true,
        // The test must not be what proved the object a field is read from.
        Kind::LoadOwnField { .. } => {
            let object = data.inputs[0];
            !condition
                .iter()
                .any(|&operand| tests_object(graph, operand, object, 3))
        }
        _ => false,
    }
}

/// The arms of the diamond `block` ends in and their merge, when it is one
/// that converts.
fn diamond(graph: &Graph, block: BlockId) -> Option<(BranchKind, BlockId, BlockId, BlockId)> {
    let control = graph.block(block).control?;
    let Kind::Branch {
        kind,
        if_true,
        if_false,
    } = graph.node(control).kind
    else {
        return None;
    };
    if if_true == if_false {
        return None;
    }
    let inputs = &graph.node(control).inputs;
    let operands = condition_operands(graph, kind, inputs)?;
    let condition = &inputs[..operands];
    let merge_of = |arm: BlockId| {
        let data = graph.block(arm);
        if data.predecessors.as_slice() != [block]
            || !data.phis.is_empty()
            || data.body.len() > ARM_NODES
            || !data
                .body
                .iter()
                .all(|&node| runs_early(graph, node, condition))
        {
            return None;
        }
        match graph.node(data.control?).kind {
            Kind::Jump(merge) => Some(merge),
            _ => None,
        }
    };
    let merge = merge_of(if_true)?;
    if merge_of(if_false)? != merge || merge == block {
        return None;
    }
    let merged = graph.block(merge);
    if merged.is_loop
        || merged.predecessors.len() != 2
        || !merged.predecessors.contains(&if_true)
        || !merged.predecessors.contains(&if_false)
        || merged.phis.is_empty()
        || !merged.phis.iter().all(|&phi| {
            matches!(
                graph.node(phi).repr,
                Repr::Tagged | Repr::Int32 | Repr::Float64
            )
        })
    {
        return None;
    }
    Some((kind, if_true, if_false, merge))
}

/// Replace every diamond of `layout` that only picks values with selects.
/// Returns how many diamonds converted.
pub(crate) fn convert(graph: &mut Graph, layout: &mut Vec<BlockId>) -> usize {
    let mut removed: Vec<BlockId> = Vec::new();
    for &block in layout.iter() {
        if removed.contains(&block) {
            continue;
        }
        let Some((kind, if_true, if_false, merge)) = diamond(graph, block) else {
            continue;
        };
        let control = graph.block(block).control.expect("a branch");
        let inputs = graph.node(control).inputs.clone();
        // Both arms run in the testing block, in order, before the selects.
        for arm in [if_true, if_false] {
            let body = std::mem::take(&mut graph.block_mut(arm).body);
            for &node in &body {
                graph.node_mut(node).block = Some(block);
            }
            graph.block_mut(block).body.extend(body);
        }
        let true_index = graph
            .block(merge)
            .predecessors
            .iter()
            .position(|&predecessor| predecessor == if_true)
            .expect("the true arm enters the merge");
        let phis = std::mem::take(&mut graph.block_mut(merge).phis);
        for phi in phis {
            let data = graph.node(phi);
            let repr = data.repr;
            let (picked, other) = (data.inputs[true_index], data.inputs[1 - true_index]);
            let mut operands: SmallVec<[NodeId; 3]> = inputs.clone();
            operands.push(picked);
            operands.push(other);
            let select = graph.node_mut(phi);
            select.kind = Kind::Select(kind);
            select.inputs = operands;
            select.repr = repr;
            select.block = Some(block);
            graph.block_mut(block).body.push(phi);
        }
        let jump = graph.node_mut(control);
        jump.kind = Kind::Jump(merge);
        jump.inputs.clear();
        graph.block_mut(merge).predecessors = vec![block];
        for arm in [if_true, if_false] {
            let data = graph.block_mut(arm);
            data.control = None;
            data.predecessors.clear();
            removed.push(arm);
        }
    }
    layout.retain(|block| !removed.contains(block));
    removed.len() / 2
}

#[cfg(test)]
#[path = "select_tests.rs"]
mod tests;
