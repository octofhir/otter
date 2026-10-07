//! Phi representation selection: a phi whose every input is an int32 value
//! becomes an unboxed int32 phi, and one whose every input is a number an
//! unboxed float64 phi (Maglev's phi untagging).
//!
//! # Contents
//! - [`untag_phis`] — retype qualifying phis, drop the checks that unboxed
//!   them, and box them only where a use needs a tagged value.
//!
//! # Invariants
//! - Int32 untagging runs first; float64 untagging then considers the phis
//!   left tagged, so a phi is float64 only when it is not int32.
//! - A phi is untagged when every input is a constant of the target
//!   representation, the box of such a value (for float64, of an int32 value
//!   too, converted at the end of its predecessor), another phi being
//!   untagged, or — for a loop-header phi the loop body unboxes to the target
//!   — a tagged value entering the loop along a forward edge. Such an entry
//!   value is checked at the end of its predecessor, whose eager deopt
//!   resumes the interpreter at the loop header with that edge's values; a
//!   header that already left optimized code for a type mismatch speculates
//!   on no entry value.
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

/// One unboxed representation a phi may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Int32,
    Float64,
}

impl Target {
    fn repr(self) -> Repr {
        match self {
            Self::Int32 => Repr::Int32,
            Self::Float64 => Repr::Float64,
        }
    }

    /// The box that turns the target back into a tagged value.
    fn box_kind(self) -> Kind {
        match self {
            Self::Int32 => Kind::Int32ToTagged,
            Self::Float64 => Kind::Float64ToTagged,
        }
    }

    /// The check that unboxes a tagged value to the target.
    fn check_kind(self) -> Kind {
        match self {
            Self::Int32 => Kind::CheckedTaggedToInt32,
            Self::Float64 => Kind::CheckedTaggedToFloat64,
        }
    }

    /// Whether `kind` unboxes its input to the target, so the target phi
    /// itself stands for it.
    fn unboxes(self, kind: &Kind) -> bool {
        match self {
            // An int32 phi is its own int32 form and its own element index.
            Self::Int32 => matches!(
                kind,
                Kind::CheckedTaggedToInt32 | Kind::CheckedTaggedToIndex
            ),
            Self::Float64 => matches!(kind, Kind::CheckedTaggedToFloat64),
        }
    }

    /// Whether `node` is a constant or a box this target unboxes for free.
    fn source(self, graph: &Graph, node: NodeId) -> bool {
        match (self, &graph.node(node).kind) {
            (Self::Int32, Kind::ConstTagged(bits)) => {
                bits & otter_vm::value::tag::NUMBER_TAG == otter_vm::value::tag::NUMBER_TAG
            }
            (Self::Float64, Kind::ConstTagged(bits)) => {
                otter_vm::Value::from_bits(*bits).as_number().is_some()
            }
            (_, Kind::Int32ToTagged) | (Self::Float64, Kind::Float64ToTagged) => true,
            _ => false,
        }
    }

    /// The target form of the source `input` arriving from `predecessor`.
    fn unbox_source(self, graph: &mut Graph, input: NodeId, predecessor: BlockId) -> NodeId {
        let data = graph.node(input);
        match (self, data.kind.clone()) {
            (Self::Int32, Kind::ConstTagged(bits)) => {
                graph.constant(Kind::ConstInt32(bits as u32 as i32))
            }
            (Self::Float64, Kind::ConstTagged(bits)) => {
                let number = otter_vm::Value::from_bits(bits)
                    .as_number()
                    .expect("a number constant");
                graph.constant(Kind::ConstFloat64(number.as_f64().to_bits()))
            }
            (Self::Int32, Kind::Int32ToTagged) | (Self::Float64, Kind::Float64ToTagged) => {
                data.inputs[0]
            }
            (Self::Float64, Kind::Int32ToTagged) => {
                let int = data.inputs[0];
                let converted = graph.add_node(Kind::Int32ToFloat64, &[int], Repr::Float64);
                graph.node_mut(converted).block = Some(predecessor);
                graph.block_mut(predecessor).body.push(converted);
                converted
            }
            _ => unreachable!("not a source of this representation"),
        }
    }
}

/// Untag every qualifying phi of `graph`: to int32, then to float64.
pub(crate) fn untag_phis(graph: &mut Graph, layout: &[BlockId], loop_headers: &[LoopHeader]) {
    untag(graph, layout, loop_headers, Target::Int32);
    untag(graph, layout, loop_headers, Target::Float64);
}

/// Untag every tagged phi that qualifies for `target`.
fn untag(graph: &mut Graph, layout: &[BlockId], loop_headers: &[LoopHeader], target: Target) {
    let phis: Vec<NodeId> = layout
        .iter()
        .flat_map(|&block| graph.block(block).phis.clone())
        .filter(|&phi| graph.node(phi).repr == Repr::Tagged)
        .collect();
    if phis.is_empty() {
        return;
    }
    // Entry edges of loop-header phis the body unboxes to the target may be
    // checked in their predecessor.
    let mut target_used: FxHashSet<NodeId> = FxHashSet::default();
    for &block in layout {
        for &node in &graph.block(block).body {
            let data = graph.node(node);
            if target.unboxes(&data.kind) {
                target_used.insert(data.inputs[0]);
            }
        }
    }
    let mut speculative: FxHashSet<(NodeId, usize)> = FxHashSet::default();
    let mut header_of: FxHashMap<NodeId, usize> = FxHashMap::default();
    for (header_index, header) in loop_headers.iter().enumerate() {
        for &(_, phi) in &header.phis {
            header_of.insert(phi, header_index);
            if !header.speculate || !target_used.contains(&phi) {
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
                        || target.source(graph, input)
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
    // Every phi of the layout, since a header may hold phis an earlier pass
    // already untagged.
    let original: FxHashMap<NodeId, smallvec::SmallVec<[NodeId; 3]>> = layout
        .iter()
        .flat_map(|&block| graph.block(block).phis.iter().copied())
        .map(|phi| (phi, graph.node(phi).inputs.clone()))
        .collect();
    let mut entry_checks: FxHashMap<(BlockId, NodeId), NodeId> = FxHashMap::default();
    for &phi in &phis {
        if !candidates.contains(&phi) {
            continue;
        }
        let inputs = graph.node(phi).inputs.clone();
        let block = graph.node(phi).block.expect("a placed phi");
        let mut untagged = smallvec::SmallVec::<[NodeId; 3]>::new();
        for (index, input) in inputs.into_iter().enumerate() {
            let predecessor = graph.block(block).predecessors[index];
            let replacement = if candidates.contains(&input) {
                input
            } else if target.source(graph, input) {
                target.unbox_source(graph, input, predecessor)
            } else {
                debug_assert!(speculative.contains(&(phi, index)));
                let header = &loop_headers[header_of[&phi]];
                *entry_checks.entry((predecessor, input)).or_insert_with(|| {
                    entry_check(graph, header, &original, index, predecessor, input, target)
                })
            };
            untagged.push(replacement);
        }
        let node = graph.node_mut(phi);
        node.inputs = untagged;
        node.repr = target.repr();
    }
    // Checks that unboxed an untagged phi become the phi.
    let mut replaced: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    for &block in layout {
        for &node in &graph.block(block).body {
            let data = graph.node(node);
            if target.unboxes(&data.kind) && candidates.contains(&data.inputs[0]) {
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
                // A use of the unboxing check already reads the unboxed
                // value; a direct use of the phi read it tagged and gets a
                // box.
                if let Some(&phi) = replaced.get(&original) {
                    rewired[index] = phi;
                    continue;
                }
                if !candidates.contains(&original) {
                    continue;
                }
                let boxed = *boxes.entry(original).or_insert_with(|| {
                    let boxed = graph.add_node(target.box_kind(), &[original], Repr::Tagged);
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

/// A `target` check of the tagged entry value `input` at the end of
/// `predecessor`, the `index`th predecessor of `header`. Its eager deopt
/// resumes the interpreter at the header with this edge's values.
fn entry_check(
    graph: &mut Graph,
    header: &LoopHeader,
    original: &FxHashMap<NodeId, smallvec::SmallVec<[NodeId; 3]>>,
    index: usize,
    predecessor: BlockId,
    input: NodeId,
    target: Target,
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
    let check = graph.add_node(target.check_kind(), &[input], target.repr());
    let node = graph.node_mut(check);
    node.block = Some(predecessor);
    node.eager = Some(state);
    graph.block_mut(predecessor).body.push(check);
    check
}
