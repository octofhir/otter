//! Branches on a merged boolean take the merged edges directly.
//!
//! `if (a || b)`, and an inlined `return x != 0 || y == 1` tested by its
//! caller, merge one boolean per incoming edge into a phi that a truthiness
//! branch then reads. Every incoming value already decides which way the
//! branch goes, so each predecessor can branch on its own value (or, for a
//! `true`/`false` constant, jump) to where the branch's edges lead, and the
//! merge never materializes. An int32 compare a predecessor branches on fuses
//! into the branch itself, and a predecessor that sits on an edge of a branch
//! on the same value already knows it and jumps.
//!
//! # Contents
//! - [`thread_boolean_phis`] — the rewrite over every truthiness branch.
//!
//! # Invariants
//! - Only a phi the branch alone reads is threaded, and only when its block
//!   holds no other phi and no body, and reaches the branch through blocks
//!   that hold nothing but a jump. The phi's value is then observable only as
//!   the edge the branch takes.
//! - Each of the branch's edges must lead through an empty block that only
//!   it enters to a block that is no loop header. New edges go straight to
//!   that block, from a predecessor ending in a jump or from a fresh block
//!   only the predecessor's new branch enters, and every phi there reads on
//!   the new edge what it read on the bypassed one. No branch ever targets a
//!   block another edge enters, so no edge needs moves a branch cannot hold.
//! - A predecessor is threaded only when it ends in a plain jump to the phi's
//!   block; the others keep their edges and phi inputs. A phi block left
//!   without predecessors leaves the layout with its chain, the branch and
//!   the two bypassed blocks.
//! - Values the bypassed edges carried were defined above the phi's block,
//!   which every one of its predecessors is below.
//!
//! # See also
//! - [`super::branch_fold`] — strips `!` and ToBoolean under a branch first.

use otter_vm::value::tag;

use super::ir::{BlockId, BranchKind, Graph, Kind, NodeId, Repr};

/// Thread every truthiness branch on a boolean phi through the phi's
/// predecessors, updating `layout` with the blocks it adds and drops.
pub(crate) fn thread_boolean_phis(graph: &mut Graph, layout: &mut Vec<BlockId>) {
    let mut uses = vec![0u32; graph.nodes.len()];
    for node in &graph.nodes {
        for input in &node.inputs {
            uses[input.0 as usize] += 1;
        }
    }
    for state in &mut graph.frame_states {
        state.for_each_value_mut(|value| uses[value.0 as usize] += 1);
    }
    let blocks: Vec<BlockId> = layout.clone();
    for block in blocks {
        thread_one(graph, layout, &uses, block);
    }
}

/// One edge of the threaded branch: the empty block it enters and the block
/// that one jumps to.
#[derive(Clone, Copy)]
struct Edge {
    split: BlockId,
    target: BlockId,
}

/// The edge through `split`, when `split` is an empty block only `branch`
/// enters, jumping to a block that is no loop header.
fn edge(graph: &Graph, branch: BlockId, split: BlockId) -> Option<Edge> {
    let data = graph.block(split);
    if data.predecessors.as_slice() != [branch] || !data.phis.is_empty() || !data.body.is_empty() {
        return None;
    }
    let Kind::Jump(target) = graph.node(data.control?).kind else {
        return None;
    };
    (!graph.block(target).is_loop).then_some(Edge { split, target })
}

/// The blocks from `merge` to `branch` when every block after `merge` holds
/// nothing but a jump and is entered only from the one before it.
fn jump_chain(graph: &Graph, merge: BlockId, branch: BlockId) -> Option<Vec<BlockId>> {
    let mut chain = Vec::new();
    let mut current = merge;
    while current != branch {
        let control = graph.block(current).control?;
        let Kind::Jump(next) = graph.node(control).kind else {
            return None;
        };
        let data = graph.block(next);
        if data.predecessors.as_slice() != [current]
            || !data.phis.is_empty()
            || data.is_loop
            || (next != branch && !data.body.is_empty())
            || chain.len() > 8
        {
            return None;
        }
        chain.push(next);
        current = next;
    }
    Some(chain)
}

fn thread_one(graph: &mut Graph, layout: &mut Vec<BlockId>, uses: &[u32], block: BlockId) {
    let Some(control) = graph.block(block).control else {
        return;
    };
    let Kind::Branch {
        kind: BranchKind::Truthy,
        if_true,
        if_false,
    } = graph.node(control).kind
    else {
        return;
    };
    let phi = graph.node(control).inputs[0];
    if !matches!(graph.node(phi).kind, Kind::Phi) || uses[phi.0 as usize] != 1 {
        return;
    }
    let Some(merge) = graph.node(phi).block else {
        return;
    };
    let merge_data = graph.block(merge);
    if merge_data.phis.as_slice() != [phi]
        || !merge_data.body.is_empty()
        || merge_data.is_loop
        || !graph.block(block).body.is_empty()
    {
        return;
    }
    let (Some(on_true), Some(on_false)) =
        (edge(graph, block, if_true), edge(graph, block, if_false))
    else {
        return;
    };
    if on_true.target == merge || on_false.target == merge {
        return;
    }
    let Some(chain) = jump_chain(graph, merge, block) else {
        return;
    };
    let predecessors = graph.block(merge).predecessors.clone();
    let inputs: Vec<NodeId> = graph.node(phi).inputs.to_vec();
    if predecessors.len() != inputs.len() {
        return;
    }
    let mut kept_predecessors = Vec::new();
    let mut kept_inputs = Vec::new();
    for (&predecessor, &value) in predecessors.iter().zip(&inputs) {
        if !thread_edge(graph, layout, predecessor, merge, value, on_true, on_false) {
            kept_predecessors.push(predecessor);
            kept_inputs.push(value);
        }
    }
    if kept_predecessors.len() == predecessors.len() {
        return;
    }
    if kept_predecessors.is_empty() {
        // Nothing enters the merge any more: it, its chain, the branch and
        // both bypassed blocks leave the layout and their targets' edges.
        graph.block_mut(merge).phis.clear();
        graph.block_mut(merge).predecessors.clear();
        graph.node_mut(phi).inputs.clear();
        for gone in [on_true, on_false] {
            remove_edge(graph, gone.split, gone.target);
            graph.block_mut(gone.split).predecessors.clear();
        }
        layout.retain(|&id| {
            id != merge && !chain.contains(&id) && id != on_true.split && id != on_false.split
        });
        return;
    }
    graph.block_mut(merge).predecessors = kept_predecessors;
    graph.node_mut(phi).inputs = kept_inputs.into_iter().collect();
}

/// Redirect `predecessor`'s jump into `merge` by the edge `value` decides.
/// `false` when the edge stays as it was.
fn thread_edge(
    graph: &mut Graph,
    layout: &mut Vec<BlockId>,
    predecessor: BlockId,
    merge: BlockId,
    value: NodeId,
    on_true: Edge,
    on_false: Edge,
) -> bool {
    let Some(jump) = graph.block(predecessor).control else {
        return false;
    };
    if graph.node(jump).kind != Kind::Jump(merge) || predecessor == merge {
        return false;
    }
    let value_kind = graph.node(value).kind.clone();
    let known = match value_kind {
        Kind::ConstTagged(bits) if bits == tag::VALUE_TRUE => Some(true),
        Kind::ConstTagged(bits) if bits == tag::VALUE_FALSE => Some(false),
        _ => known_on_entry(graph, predecessor, value),
    };
    if let Some(known) = known {
        let taken = if known { on_true } else { on_false };
        graph.node_mut(jump).kind = Kind::Jump(taken.target);
        add_edge(graph, predecessor, taken);
        return true;
    }
    if !value_kind.produces_boolean() {
        return false;
    }
    let (kind, inputs) = match value_kind {
        Kind::Int32Compare(condition) => (
            BranchKind::Int32(condition),
            graph.node(value).inputs.clone(),
        ),
        _ => (BranchKind::Truthy, [value].into_iter().collect()),
    };
    let split_true = split_edge(graph, jump, predecessor, on_true);
    let split_false = split_edge(graph, jump, predecessor, on_false);
    let node = graph.node_mut(jump);
    node.kind = Kind::Branch {
        kind,
        if_true: split_true,
        if_false: split_false,
    };
    node.inputs = inputs;
    let at = layout
        .iter()
        .position(|&id| id == predecessor)
        .map_or(layout.len(), |index| index + 1);
    layout.splice(at..at, [split_true, split_false]);
    true
}

/// Enter `edge.target` from `from` as well, every phi there reading on the
/// new edge what it reads on the edge from `edge.split`.
fn add_edge(graph: &mut Graph, from: BlockId, edge: Edge) {
    let target = graph.block(edge.target);
    let index = target
        .predecessors
        .iter()
        .position(|&predecessor| predecessor == edge.split)
        .expect("the bypassed block enters its target");
    let phis = target.phis.clone();
    for phi in phis {
        let value = graph.node(phi).inputs[index];
        graph.node_mut(phi).inputs.push(value);
    }
    graph.block_mut(edge.target).predecessors.push(from);
}

/// Drop the edge from `from` into `target` with the phi inputs it carries.
fn remove_edge(graph: &mut Graph, from: BlockId, target: BlockId) {
    let Some(index) = graph
        .block(target)
        .predecessors
        .iter()
        .position(|&predecessor| predecessor == from)
    else {
        return;
    };
    let phis = graph.block(target).phis.clone();
    for phi in phis {
        graph.node_mut(phi).inputs.remove(index);
    }
    graph.block_mut(target).predecessors.remove(index);
}

/// The truthiness of `value` on entry to `block`, when the nearest branch
/// above it, through blocks entered only from the one before, tested `value`
/// (or fused its int32 compare) and `block` lies on one of its edges.
fn known_on_entry(graph: &Graph, block: BlockId, value: NodeId) -> Option<bool> {
    let mut edge = block;
    for _ in 0..8 {
        let [above] = graph.block(edge).predecessors[..] else {
            return None;
        };
        let control = graph.block(above).control?;
        let node = graph.node(control);
        match node.kind {
            Kind::Jump(_) => edge = above,
            Kind::Branch {
                kind,
                if_true,
                if_false,
            } => {
                let tested = match kind {
                    BranchKind::Truthy => node.inputs[0] == value,
                    BranchKind::Int32(condition) => {
                        graph.node(value).kind == Kind::Int32Compare(condition)
                            && graph.node(value).inputs == node.inputs
                    }
                    _ => false,
                };
                return match (tested, edge == if_true, edge == if_false) {
                    (true, true, false) => Some(true),
                    (true, false, true) => Some(false),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
    None
}

/// A fresh block only `predecessor` enters, jumping to `edge.target`, its
/// jump sharing the source position of `jump`.
fn split_edge(graph: &mut Graph, jump: NodeId, predecessor: BlockId, edge: Edge) -> BlockId {
    let block = graph.new_block();
    let (pc, origin) = (graph.node(jump).pc, graph.node(jump).origin);
    let node = graph.add_node(Kind::Jump(edge.target), &[], Repr::None);
    let data = graph.node_mut(node);
    data.block = Some(block);
    data.pc = pc;
    data.origin = origin;
    let new = graph.block_mut(block);
    new.predecessors.push(predecessor);
    new.control = Some(node);
    add_edge(graph, block, edge);
    block
}
