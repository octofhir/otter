//! Loop-invariant code motion: a check of a value every iteration sees
//! unchanged, and a load every iteration reads the same, run once, in a
//! pre-header every entry into the loop passes.
//!
//! # Contents
//! - [`hoist_invariants`] — per loop, innermost first: the pre-header and
//!   the nodes moved into it.
//!
//! # Invariants
//! - Code moves only out of a loop whose body neither calls out nor runs a
//!   slow path that may run JavaScript, so no object's element layout or
//!   length changes while the loop runs: generated element stores never
//!   transition a layout or grow storage, they leave instead. A shape check
//!   also needs a loop that changes no shape; a context slot load needs a
//!   loop that stores to no slot at its offset; a global binding read needs
//!   a loop that stores no object property. Only an off-heap element base
//!   moves: a raw base into a movable slab must not live across the
//!   back edge's poll. Context parents never change.
//! - A moved node reads only values defined before the loop, values moved
//!   before it, or a header phi the loop never changes, which it reads as
//!   the value entering the loop.
//! - The pre-header merges every entering edge, the OSR entry included,
//!   with a phi wherever the edges bring different values; the header's
//!   back edges stay its last predecessors.
//! - A moved check leaves to the loop header with the values entering the
//!   loop, so the interpreter runs the iteration the check guarded. A
//!   header that already left optimized code for a shape or layout mismatch
//!   moves nothing: its next compile checks where the value is used.
//!
//! # See also
//! - [`super::phi_repr`] — checks entry values of untagged phis on their
//!   entering edges.
//! - [`super::builder::LoopHeader`] — the loop facts this pass reads.

use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use super::builder::LoopHeader;
use super::ir::{BlockId, FrameState, FrameStateId, Graph, Kind, NodeId, Repr};

/// Move the invariant nodes of every loop in `loop_headers` into a
/// pre-header placed before the loop in `layout`.
pub(crate) fn hoist_invariants(
    graph: &mut Graph,
    layout: &mut Vec<BlockId>,
    loop_headers: &[LoopHeader],
) {
    let mut loops: Vec<(&LoopHeader, usize)> = loop_headers
        .iter()
        .filter(|header| header.hoist)
        .map(|header| (header, loop_body(graph, header.block).len()))
        .collect();
    // Inner loops first: what leaves an inner loop, its pre-header
    // included, may leave its outer one too.
    loops.sort_by_key(|&(_, size)| size);
    for (header, _) in loops {
        let body = loop_body(graph, header.block);
        hoist_loop(graph, layout, header, &body);
    }
}

/// The blocks of the loop headed by `header`: the header and every block
/// that reaches one of its back edges without passing the header.
fn loop_body(graph: &Graph, header: BlockId) -> FxHashSet<BlockId> {
    let mut body = FxHashSet::default();
    body.insert(header);
    let mut work: Vec<BlockId> = graph
        .block(header)
        .predecessors
        .iter()
        .copied()
        .filter(|&predecessor| {
            graph.block(predecessor).control.is_some_and(|control| {
                matches!(graph.node(control).kind, Kind::JumpLoop(target) if target == header)
            })
        })
        .collect();
    while let Some(block) = work.pop() {
        if body.insert(block) {
            work.extend(graph.block(block).predecessors.iter().copied());
        }
    }
    body
}

/// What the loop body may change.
struct Clobbers {
    shapes: bool,
    lengths: bool,
    /// Whether the loop stores any object property, the global object's
    /// included.
    properties: bool,
    slot_offsets: FxHashSet<i32>,
}

fn clobbers(graph: &Graph, body: &FxHashSet<BlockId>) -> Option<Clobbers> {
    let mut clobbers = Clobbers {
        shapes: false,
        lengths: false,
        properties: false,
        slot_offsets: FxHashSet::default(),
    };
    for &block in body {
        for &node in &graph.block(block).body {
            let kind = &graph.node(node).kind;
            let properties = kind.properties();
            if properties.call || properties.may_collect {
                return None;
            }
            match kind {
                Kind::StoreNamedProperty(_)
                | Kind::StorePropertyCached { .. }
                | Kind::StoreKeyedCached { .. } => {
                    clobbers.shapes = true;
                    clobbers.lengths = true;
                    clobbers.properties = true;
                }
                Kind::StoreOwnField(_) => clobbers.properties = true,
                Kind::StoreTaggedField(offset) => {
                    clobbers.slot_offsets.insert(*offset);
                }
                _ => {}
            }
        }
    }
    Some(clobbers)
}

fn hoist_loop(
    graph: &mut Graph,
    layout: &mut Vec<BlockId>,
    header: &LoopHeader,
    body: &FxHashSet<BlockId>,
) {
    let Some(clobbers) = clobbers(graph, body) else {
        return;
    };
    let predecessors = graph.block(header.block).predecessors.clone();
    let entering: SmallVec<[usize; 2]> = (0..predecessors.len())
        .filter(|&index| !body.contains(&predecessors[index]))
        .collect();
    if entering.is_empty() {
        return;
    }
    // A header phi the loop never changes holds its entering value.
    let unchanged_phi = |graph: &Graph, value: NodeId| -> bool {
        let data = graph.node(value);
        data.kind == Kind::Phi
            && data.block == Some(header.block)
            && data
                .inputs
                .iter()
                .enumerate()
                .all(|(index, &input)| entering.contains(&index) || input == value)
    };
    let ordered: Vec<BlockId> = layout
        .iter()
        .copied()
        .filter(|block| body.contains(block))
        .collect();
    let mut moved: Vec<NodeId> = Vec::new();
    let mut moved_set: FxHashSet<NodeId> = FxHashSet::default();
    let mut checks: Vec<(Kind, SmallVec<[NodeId; 3]>)> = Vec::new();
    for &block in &ordered {
        let nodes = graph.block(block).body.clone();
        let mut kept = Vec::with_capacity(nodes.len());
        for node in nodes {
            let data = graph.node(node);
            let movable = match &data.kind {
                Kind::CheckElements { .. } | Kind::LoadContextParent => true,
                Kind::CheckShapes { .. } => !clobbers.shapes,
                Kind::LoadElementsLength { .. } => !clobbers.lengths,
                // A movable slab base must not live across the loop's poll.
                Kind::LoadElementsBase { off_heap, .. } => *off_heap && !clobbers.lengths,
                Kind::LoadGlobalBinding(_) => !clobbers.properties,
                Kind::LoadTaggedField(offset) => {
                    data.inputs
                        .first()
                        .is_some_and(|&input| graph.node(input).repr == Repr::Tagged)
                        && !clobbers.slot_offsets.contains(offset)
                }
                _ => false,
            };
            let invariant = movable
                && data.inputs.iter().all(|&input| {
                    let input_data = graph.node(input);
                    input_data.block.is_none_or(|block| !body.contains(&block))
                        || moved_set.contains(&input)
                        || unchanged_phi(graph, input)
                });
            if !invariant {
                kept.push(node);
                continue;
            }
            if data.repr == Repr::None {
                let key = (data.kind.clone(), data.inputs.clone());
                if checks.contains(&key) {
                    continue;
                }
                checks.push(key);
            }
            moved_set.insert(node);
            moved.push(node);
        }
        graph.block_mut(block).body = kept;
    }
    if moved.is_empty() {
        return;
    }
    let preheader = insert_preheader(graph, layout, header, &entering);
    let state = entry_state(graph, header);
    for node in moved {
        // A header phi the loop never changes is, here, the value the
        // pre-header brings it: its first input.
        let inputs = graph.node(node).inputs.clone();
        let rewired: SmallVec<[NodeId; 3]> = inputs
            .iter()
            .map(|&input| {
                let data = graph.node(input);
                let unchanged = data.kind == Kind::Phi
                    && data.block == Some(header.block)
                    && data.inputs[1..].iter().all(|&back| back == input);
                if unchanged { data.inputs[0] } else { input }
            })
            .collect();
        let data = graph.node_mut(node);
        data.inputs = rewired;
        data.block = Some(preheader);
        if data.eager.is_some() {
            data.eager = Some(state);
        }
        graph.block_mut(preheader).body.push(node);
    }
}

/// A block every entering edge of the loop goes through, placed just
/// before the header: the header's first predecessor, ahead of its back
/// edges, with a phi for each header phi whose entering values differ.
fn insert_preheader(
    graph: &mut Graph,
    layout: &mut Vec<BlockId>,
    header: &LoopHeader,
    entering: &[usize],
) -> BlockId {
    let preheader = graph.new_block();
    let predecessors = graph.block(header.block).predecessors.clone();
    let back_edges: SmallVec<[BlockId; 2]> = (0..predecessors.len())
        .filter(|index| !entering.contains(index))
        .map(|index| predecessors[index])
        .collect();
    graph.position = graph.frame_state(header.state).pc;
    graph.origin = 0;
    for phi in graph.block(header.block).phis.clone() {
        let inputs = graph.node(phi).inputs.clone();
        let entry: SmallVec<[NodeId; 2]> = entering.iter().map(|&index| inputs[index]).collect();
        let from_preheader = if entry.iter().all(|&value| value == entry[0]) {
            entry[0]
        } else {
            let repr = graph.node(phi).repr;
            let merged = graph.add_node(Kind::Phi, &entry, repr);
            graph.node_mut(merged).block = Some(preheader);
            graph.block_mut(preheader).phis.push(merged);
            merged
        };
        let mut rewired: SmallVec<[NodeId; 3]> = SmallVec::new();
        rewired.push(from_preheader);
        rewired.extend(
            (0..inputs.len())
                .filter(|index| !entering.contains(index))
                .map(|index| inputs[index]),
        );
        graph.node_mut(phi).inputs = rewired;
    }
    graph.block_mut(preheader).predecessors =
        entering.iter().map(|&index| predecessors[index]).collect();
    let mut header_predecessors = vec![preheader];
    header_predecessors.extend(back_edges);
    graph.block_mut(header.block).predecessors = header_predecessors;
    for &index in entering {
        let block = predecessors[index];
        let control = graph
            .block(block)
            .control
            .expect("an entering edge ends its block");
        match &mut graph.node_mut(control).kind {
            Kind::Jump(target) if *target == header.block => *target = preheader,
            Kind::Branch {
                if_true, if_false, ..
            } => {
                if *if_true == header.block {
                    *if_true = preheader;
                }
                if *if_false == header.block {
                    *if_false = preheader;
                }
            }
            other => unreachable!("an entering edge ends in {other:?}"),
        }
    }
    let jump = graph.add_node(Kind::Jump(header.block), &[], Repr::None);
    graph.node_mut(jump).block = Some(preheader);
    graph.block_mut(preheader).control = Some(jump);
    let position = layout
        .iter()
        .position(|&block| block == header.block)
        .expect("the header is laid out");
    layout.insert(position, preheader);
    preheader
}

/// The interpreter state at the loop header as the pre-header enters it:
/// the back edge's state with each phi bound to the pre-header's input.
fn entry_state(graph: &mut Graph, header: &LoopHeader) -> FrameStateId {
    let back_edge = graph.frame_state(header.state).clone();
    let registers = back_edge
        .registers
        .iter()
        .map(|&(register, value)| {
            let entry_value = header
                .phis
                .iter()
                .find(|&&(phi_register, _)| phi_register == register)
                .map_or(value, |&(_, phi)| graph.node(phi).inputs[0]);
            (register, entry_value)
        })
        .collect();
    graph.add_frame_state(FrameState {
        registers,
        ..back_edge
    })
}
