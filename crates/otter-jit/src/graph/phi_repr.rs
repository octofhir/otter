//! Phi representation selection: a phi whose every input is an int32 value
//! becomes an unboxed int32 phi.
//!
//! # Contents
//! - [`untag_phis`] — retype qualifying phis, drop the checks that unboxed
//!   them, and box them only where a use needs a tagged value.
//!
//! # Invariants
//! - A phi is untagged only when every input is an int32 constant, the box
//!   of an int32 value, or another phi being untagged; the result is the
//!   same number on every path, so no speculation is added.
//! - A tagged phi never takes an untagged phi as input: such a phi is not
//!   untagged.
//! - Every check that unboxed an untagged phi is replaced by the phi itself,
//!   in node inputs and frame states alike; every other use receives a box
//!   placed in its own block, right before it.
//!
//! # See also
//! - [`super::builder`] — creates every phi tagged.

use rustc_hash::{FxHashMap, FxHashSet};

use super::ir::{BlockId, Graph, Kind, NodeId, Repr};

/// Untag every qualifying phi of `graph`.
pub(crate) fn untag_phis(graph: &mut Graph, layout: &[BlockId]) {
    let phis: Vec<NodeId> = layout
        .iter()
        .flat_map(|&block| graph.block(block).phis.clone())
        .filter(|&phi| graph.node(phi).repr == Repr::Tagged)
        .collect();
    if phis.is_empty() {
        return;
    }
    // Phi uses by other phis, to keep a tagged phi from reading an untagged
    // one.
    let mut candidates: FxHashSet<NodeId> = phis.iter().copied().collect();
    loop {
        let mut changed = false;
        for &phi in &phis {
            if !candidates.contains(&phi) {
                continue;
            }
            let qualifies = graph.node(phi).inputs.iter().all(|&input| {
                candidates.contains(&input)
                    || match graph.node(input).kind {
                        Kind::ConstTagged(bits) => {
                            bits & otter_vm::value::tag::NUMBER_TAG
                                == otter_vm::value::tag::NUMBER_TAG
                        }
                        Kind::Int32ToTagged => true,
                        _ => false,
                    }
            });
            if !qualifies {
                candidates.remove(&phi);
                changed = true;
            }
        }
        for &phi in &phis {
            if candidates.contains(&phi) {
                continue;
            }
            // A remaining tagged phi must not read an untagged one.
            let readers: Vec<NodeId> = graph
                .node(phi)
                .inputs
                .iter()
                .copied()
                .filter(|input| candidates.contains(input))
                .collect();
            for input in readers {
                candidates.remove(&input);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if candidates.is_empty() {
        return;
    }
    // Retype and rewire the inputs.
    for &phi in &phis {
        if !candidates.contains(&phi) {
            continue;
        }
        let inputs = graph.node(phi).inputs.clone();
        let mut untagged = smallvec::SmallVec::<[NodeId; 3]>::new();
        for input in inputs {
            let replacement = if candidates.contains(&input) {
                input
            } else {
                match graph.node(input).kind {
                    Kind::ConstTagged(bits) => graph.constant(Kind::ConstInt32(bits as u32 as i32)),
                    Kind::Int32ToTagged => graph.node(input).inputs[0],
                    _ => unreachable!("a qualifying phi input"),
                }
            };
            untagged.push(replacement);
        }
        let node = graph.node_mut(phi);
        node.inputs = untagged;
        node.repr = Repr::Int32;
    }
    // Checks that unboxed an untagged phi become the phi.
    let mut replaced: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    for &block in layout {
        for &node in &graph.block(block).body {
            let data = graph.node(node);
            if data.kind == Kind::CheckedTaggedToInt32 && candidates.contains(&data.inputs[0]) {
                replaced.insert(node, data.inputs[0]);
            }
        }
    }
    for &block in layout {
        let body = graph.block(block).body.clone();
        let mut rebuilt = Vec::with_capacity(body.len());
        let mut boxes: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        let control = graph.block(block).control;
        for node in body.into_iter().chain(control) {
            if replaced.contains_key(&node) {
                continue;
            }
            let inputs = graph.node(node).inputs.clone();
            let mut rewired = inputs.clone();
            for (index, &input) in inputs.iter().enumerate() {
                let input = replaced.get(&input).copied().unwrap_or(input);
                rewired[index] = input;
                if !candidates.contains(&input) {
                    continue;
                }
                let kind = &graph.node(node).kind;
                // Int32 consumers read the phi itself.
                if node_reads_int32(graph, kind, node, index) {
                    continue;
                }
                let boxed = *boxes.entry(input).or_insert_with(|| {
                    let boxed = graph.add_node(Kind::Int32ToTagged, &[input], Repr::Tagged);
                    graph.node_mut(boxed).block = Some(block);
                    rebuilt.push(boxed);
                    boxed
                });
                rewired[index] = boxed;
            }
            graph.node_mut(node).inputs = rewired;
            if Some(node) != control {
                rebuilt.push(node);
            }
        }
        graph.block_mut(block).body = rebuilt;
    }
    // Phi inputs read replaced checks too.
    for &phi in &phis {
        let inputs = graph.node(phi).inputs.clone();
        let rewired = inputs
            .iter()
            .map(|input| replaced.get(input).copied().unwrap_or(*input))
            .collect();
        graph.node_mut(phi).inputs = rewired;
    }
    for state in &mut graph.frame_states {
        for (_, value) in &mut state.registers {
            if let Some(&replacement) = replaced.get(value) {
                *value = replacement;
            }
        }
    }
}

/// Whether input `index` of `node` already expects an int32 value.
fn node_reads_int32(graph: &Graph, kind: &Kind, node: NodeId, index: usize) -> bool {
    let _ = index;
    match kind {
        Kind::Phi => graph.node(node).repr == Repr::Int32,
        Kind::Int32Add
        | Kind::Int32Sub
        | Kind::Int32Mul
        | Kind::Int32Div
        | Kind::Int32Mod
        | Kind::Int32Negate
        | Kind::Int32BitAnd
        | Kind::Int32BitOr
        | Kind::Int32BitXor
        | Kind::Int32BitNot
        | Kind::Int32ShiftLeft
        | Kind::Int32ShiftRight
        | Kind::Int32ShiftRightLogical
        | Kind::Uint32ShiftRightToFloat64
        | Kind::Int32Compare(_)
        | Kind::Int32ToTagged
        | Kind::Int32ToFloat64
        | Kind::CheckBounds => true,
        Kind::Branch {
            kind: super::ir::BranchKind::Int32(_),
            ..
        } => true,
        _ => false,
    }
}
