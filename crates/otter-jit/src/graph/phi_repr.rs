//! Phi representation selection: a phi whose every input is an int32 value
//! becomes an unboxed int32 phi.
//!
//! # Contents
//! - [`untag_phis`] — retype qualifying phis, drop the checks that unboxed
//!   them, and box them only where a use needs a tagged value.
//!
//! # Invariants
//! - A phi is untagged when every input is an int32 constant, the box of an
//!   int32 value, another phi being untagged, or — for a loop-header phi the
//!   loop body unboxes to an int32 — a tagged value entering the loop along a
//!   forward edge. Such an entry value is checked at the end of its
//!   predecessor, whose eager deopt resumes the interpreter at the loop
//!   header with that edge's values; a header that already left optimized
//!   code for a type mismatch speculates on no entry value.
//! - A tagged phi never takes an untagged phi as input: such a phi is not
//!   untagged.
//! - Every check that unboxed an untagged phi (to an int32 or an element
//!   index) is replaced by the phi itself, in node inputs and frame states
//!   alike; every direct use of the phi read it tagged and receives a box
//!   placed in its own block, right before it. A Number check of such a box
//!   is dropped.
//!
//! # See also
//! - [`super::builder`] — creates every phi tagged and records the loop
//!   headers.

use rustc_hash::{FxHashMap, FxHashSet};

use super::builder::LoopHeader;
use super::ir::{BlockId, FrameState, Graph, Kind, NodeId, Repr};

/// Whether `node` is an int32 constant or the box of an int32 value.
fn int32_source(graph: &Graph, node: NodeId) -> bool {
    match graph.node(node).kind {
        Kind::ConstTagged(bits) => {
            bits & otter_vm::value::tag::NUMBER_TAG == otter_vm::value::tag::NUMBER_TAG
        }
        Kind::Int32ToTagged => true,
        _ => false,
    }
}

/// Untag every qualifying phi of `graph`.
pub(crate) fn untag_phis(graph: &mut Graph, layout: &[BlockId], loop_headers: &[LoopHeader]) {
    let phis: Vec<NodeId> = layout
        .iter()
        .flat_map(|&block| graph.block(block).phis.clone())
        .filter(|&phi| graph.node(phi).repr == Repr::Tagged)
        .collect();
    if phis.is_empty() {
        return;
    }
    // Entry edges of loop-header phis the body unboxes to an int32 may be
    // checked in their predecessor.
    let mut int_used: FxHashSet<NodeId> = FxHashSet::default();
    for &block in layout {
        for &node in &graph.block(block).body {
            let data = graph.node(node);
            if matches!(
                data.kind,
                Kind::CheckedTaggedToInt32 | Kind::CheckedTaggedToIndex
            ) {
                int_used.insert(data.inputs[0]);
            }
        }
    }
    let mut speculative: FxHashSet<(NodeId, usize)> = FxHashSet::default();
    let mut header_of: FxHashMap<NodeId, usize> = FxHashMap::default();
    for (header_index, header) in loop_headers.iter().enumerate() {
        for &(_, phi) in &header.phis {
            header_of.insert(phi, header_index);
            if !header.speculate || !int_used.contains(&phi) {
                continue;
            }
            for (index, &predecessor) in graph.block(header.block).predecessors.iter().enumerate() {
                let back_edge = graph
                    .block(predecessor)
                    .control
                    .is_some_and(|control| matches!(graph.node(control).kind, Kind::JumpLoop(_)));
                if !back_edge {
                    speculative.insert((phi, index));
                }
            }
        }
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
            let qualifies = graph
                .node(phi)
                .inputs
                .iter()
                .enumerate()
                .all(|(index, &input)| {
                    candidates.contains(&input)
                        || int32_source(graph, input)
                        || speculative.contains(&(phi, index))
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
    // Retype and rewire the inputs. Entry checks read the original tagged
    // inputs, and their deopt states bind every phi register of the header
    // to its edge's tagged input.
    let original: FxHashMap<NodeId, smallvec::SmallVec<[NodeId; 3]>> = phis
        .iter()
        .map(|&phi| (phi, graph.node(phi).inputs.clone()))
        .collect();
    let mut entry_checks: FxHashMap<(BlockId, NodeId), NodeId> = FxHashMap::default();
    for &phi in &phis {
        if !candidates.contains(&phi) {
            continue;
        }
        let inputs = graph.node(phi).inputs.clone();
        let mut untagged = smallvec::SmallVec::<[NodeId; 3]>::new();
        for (index, input) in inputs.into_iter().enumerate() {
            let replacement = if candidates.contains(&input) {
                input
            } else {
                match graph.node(input).kind {
                    Kind::ConstTagged(bits) => graph.constant(Kind::ConstInt32(bits as u32 as i32)),
                    Kind::Int32ToTagged => graph.node(input).inputs[0],
                    _ => {
                        debug_assert!(speculative.contains(&(phi, index)));
                        let header = &loop_headers[header_of[&phi]];
                        let predecessor = graph.block(header.block).predecessors[index];
                        *entry_checks.entry((predecessor, input)).or_insert_with(|| {
                            entry_check(graph, header, &original, index, predecessor, input)
                        })
                    }
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
            // An int32 phi is its own int32 form and its own element index.
            if matches!(
                data.kind,
                Kind::CheckedTaggedToInt32 | Kind::CheckedTaggedToIndex
            ) && candidates.contains(&data.inputs[0])
            {
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
            for (index, &original) in inputs.iter().enumerate() {
                // A use of the unboxing check already reads the int32; a
                // direct use of the phi read it tagged and gets a box.
                if let Some(&phi) = replaced.get(&original) {
                    rewired[index] = phi;
                    continue;
                }
                if !candidates.contains(&original) {
                    continue;
                }
                let boxed = *boxes.entry(original).or_insert_with(|| {
                    let boxed = graph.add_node(Kind::Int32ToTagged, &[original], Repr::Tagged);
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
        state.for_each_value_mut(|value| {
            if let Some(&replacement) = replaced.get(value) {
                *value = replacement;
            }
        });
    }
    // A Number check of a value now boxed from an unboxed number holds.
    for &block in layout {
        let body = graph.block(block).body.clone();
        let kept: Vec<NodeId> = body
            .into_iter()
            .filter(|&node| {
                let data = graph.node(node);
                data.kind != Kind::CheckNumber
                    || !matches!(
                        graph.node(data.inputs[0]).kind,
                        Kind::Int32ToTagged | Kind::Float64ToTagged
                    )
            })
            .collect();
        graph.block_mut(block).body = kept;
    }
}

/// An int32 check of the tagged entry value `input` at the end of
/// `predecessor`, the `index`th predecessor of `header`. Its eager deopt
/// resumes the interpreter at the header with this edge's values.
fn entry_check(
    graph: &mut Graph,
    header: &LoopHeader,
    original: &FxHashMap<NodeId, smallvec::SmallVec<[NodeId; 3]>>,
    index: usize,
    predecessor: BlockId,
    input: NodeId,
) -> NodeId {
    let back_edge = graph.frame_state(header.state).clone();
    let registers = back_edge
        .registers
        .iter()
        .map(|&(register, value)| {
            let edge_value = header
                .phis
                .iter()
                .find(|&&(phi_register, _)| phi_register == register)
                .map_or(value, |&(_, phi)| original[&phi][index]);
            (register, edge_value)
        })
        .collect();
    let state = graph.add_frame_state(FrameState {
        registers,
        ..back_edge
    });
    // Loop headers belong to the compiled function itself.
    graph.position = back_edge.pc;
    graph.origin = 0;
    let check = graph.add_node(Kind::CheckedTaggedToInt32, &[input], Repr::Int32);
    let node = graph.node_mut(check);
    node.block = Some(predecessor);
    node.eager = Some(state);
    graph.block_mut(predecessor).body.push(check);
    check
}
