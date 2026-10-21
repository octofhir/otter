//! Constant folding, constant branches and branches a dominating branch
//! decided.
//!
//! Inlined accessors and constant fields leave int32 arithmetic and compares
//! whose operands are all constants, and branches on them: an inlined
//! `return this.mark !== 0` over a field the builder proved to hold `0`
//! compares two constants. Such a node is its constant; such a branch is a
//! jump, and the blocks only its other edge reached leave the graph. Inlined
//! bodies also test what their caller already tested (`direction == 1`
//! twice in one loop body): below the edge the first test took, the second
//! takes the same edge.
//!
//! # Contents
//! - [`fold`] — fold every constant node and decided branch of `layout`,
//!   drop the blocks no entry reaches any more, and keep the loop headers
//!   current.
//!
//! # Invariants
//! - Only operations that cannot deopt, throw or read memory fold, with the
//!   wrapping int32 semantics their nodes compute.
//! - A branch folds on constants, or below one edge of a dominating branch
//!   on the same condition and inputs: the edge's target has that branch's
//!   block as its only predecessor and dominates the folded branch, so every
//!   run that reaches it took that edge. A branch condition is a pure
//!   function of its inputs, so the edge it never takes is dead.
//! - A block loses its last predecessor only through a folded edge; it and
//!   every block that then has no predecessor (an entry never does) leave the
//!   layout with their edges. A value one of them defines is read only below
//!   it, so nothing that stays reads it.
//! - A loop header whose back edges all went keeps no loop: it is no longer a
//!   loop and leaves `loop_headers`; one that went itself leaves too.
//!
//! # See also
//! - [`super::branch_fold`] — rewrites branch inputs without changing edges.
//! - [`super::builder::remove_trivial_phis`] — merges a phi left with one
//!   distinct input.

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use otter_vm::value::tag;

use super::builder::LoopHeader;
use super::dominators::{Dominators, successors};
use super::ir::{BlockId, BranchKind, Condition, Graph, Kind, NodeId};

fn int32(graph: &Graph, node: NodeId) -> Option<i32> {
    match graph.node(node).kind {
        Kind::ConstInt32(value) => Some(value),
        _ => None,
    }
}

fn holds(condition: Condition, a: i32, b: i32) -> bool {
    match condition {
        Condition::Equal => a == b,
        Condition::NotEqual => a != b,
        Condition::Less => a < b,
        Condition::LessEqual => a <= b,
        Condition::Greater => a > b,
        Condition::GreaterEqual => a >= b,
    }
}

fn boolean(value: bool) -> Kind {
    Kind::ConstTagged(tag::OTHER_TAG | tag::BOOL_TAG | u64::from(value))
}

/// The constant `node` computes from constant inputs, if it folds.
fn folded(graph: &Graph, node: NodeId) -> Option<Kind> {
    let data = graph.node(node);
    let operand = |index: usize| int32(graph, *data.inputs.get(index)?);
    Some(match data.kind {
        Kind::Int32Compare(condition) => boolean(holds(condition, operand(0)?, operand(1)?)),
        Kind::Int32BitAnd => Kind::ConstInt32(operand(0)? & operand(1)?),
        Kind::Int32BitOr => Kind::ConstInt32(operand(0)? | operand(1)?),
        Kind::Int32BitXor => Kind::ConstInt32(operand(0)? ^ operand(1)?),
        Kind::Int32BitNot => Kind::ConstInt32(!operand(0)?),
        Kind::Int32AddWrapping => Kind::ConstInt32(operand(0)?.wrapping_add(operand(1)?)),
        Kind::Int32SubWrapping => Kind::ConstInt32(operand(0)?.wrapping_sub(operand(1)?)),
        Kind::Int32ShiftLeft => Kind::ConstInt32(operand(0)?.wrapping_shl(operand(1)? as u32 & 31)),
        Kind::Int32ShiftRight => Kind::ConstInt32(operand(0)? >> (operand(1)? as u32 & 31)),
        _ => return None,
    })
}

/// The edge a branch on constants always takes: `Some(true)` for its true
/// successor.
fn decided(graph: &Graph, control: NodeId) -> Option<bool> {
    let data = graph.node(control);
    let Kind::Branch { kind, .. } = data.kind else {
        return None;
    };
    let constant = |index: usize| Some(&graph.node(*data.inputs.get(index)?).kind);
    match kind {
        BranchKind::Int32(condition) => Some(holds(
            condition,
            int32(graph, data.inputs[0])?,
            int32(graph, data.inputs[1])?,
        )),
        BranchKind::WordEqual(expected) => match *constant(0)? {
            Kind::ConstInt32(value) => Some(value as u32 == expected),
            _ => None,
        },
        BranchKind::Truthy => match *constant(0)? {
            Kind::ConstTagged(bits) if bits == tag::OTHER_TAG | tag::BOOL_TAG | 1 => Some(true),
            Kind::ConstTagged(bits) if bits == tag::OTHER_TAG | tag::BOOL_TAG => Some(false),
            Kind::ConstInt32(value) => Some(value != 0),
            _ => None,
        },
        _ => None,
    }
}

/// The edge each branch takes because a dominating branch on the same
/// condition and inputs took one that is its target's only way in, and
/// that target dominates the branch: `true` for its true successor.
fn implied_by_dominating_branches(graph: &Graph, layout: &[BlockId]) -> FxHashMap<NodeId, bool> {
    let mut implied = FxHashMap::default();
    let branches: Vec<(BlockId, NodeId)> = layout
        .iter()
        .filter_map(|&block| {
            let control = graph.block(block).control?;
            matches!(graph.node(control).kind, Kind::Branch { .. }).then_some((block, control))
        })
        .collect();
    if branches.len() < 2 {
        return implied;
    }
    let dominators = Dominators::compute(graph, layout);
    for (block, control) in branches {
        let data = graph.node(control);
        let Kind::Branch { kind, .. } = data.kind else {
            continue;
        };
        let mut current = block;
        while let Some(parent) = dominators.immediate(current) {
            current = parent;
            let Some(parent_control) = graph.block(parent).control else {
                continue;
            };
            let parent_data = graph.node(parent_control);
            let Kind::Branch {
                kind: parent_kind,
                if_true,
                if_false,
            } = parent_data.kind
            else {
                continue;
            };
            if parent_kind != kind || parent_data.inputs != data.inputs || if_true == if_false {
                continue;
            }
            let sole_edge = |target: BlockId| {
                graph.block(target).predecessors.as_slice() == [parent]
                    && dominators.dominates(target, block)
            };
            if sole_edge(if_true) {
                implied.insert(control, true);
                break;
            }
            if sole_edge(if_false) {
                implied.insert(control, false);
                break;
            }
        }
    }
    implied
}

/// Remove the edge `from` → `target` with its phi inputs.
fn remove_edge(graph: &mut Graph, from: BlockId, target: BlockId) {
    let Some(index) = graph
        .block(target)
        .predecessors
        .iter()
        .position(|&predecessor| predecessor == from)
    else {
        return;
    };
    for phi in graph.block(target).phis.clone() {
        graph.node_mut(phi).inputs.remove(index);
    }
    graph.block_mut(target).predecessors.remove(index);
}

/// Fold every constant node and branch of `layout`. Returns how many nodes
/// and branches folded.
pub(crate) fn fold(
    graph: &mut Graph,
    layout: &mut Vec<BlockId>,
    loop_headers: &mut Vec<LoopHeader>,
) -> usize {
    let mut replaced: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    let resolve = |replaced: &FxHashMap<NodeId, NodeId>, mut node: NodeId| {
        while let Some(&next) = replaced.get(&node) {
            node = next;
        }
        node
    };
    // Values, in layout order: a folded input is a constant by the time its
    // user is visited.
    for &block in layout.iter() {
        let body = graph.block(block).body.clone();
        let mut kept = Vec::with_capacity(body.len());
        for node in body {
            let inputs: SmallVec<[NodeId; 3]> = graph
                .node(node)
                .inputs
                .iter()
                .map(|&input| resolve(&replaced, input))
                .collect();
            graph.node_mut(node).inputs = inputs;
            if let Some(kind) = folded(graph, node) {
                let constant = graph.constant(kind);
                replaced.insert(node, constant);
                continue;
            }
            kept.push(node);
        }
        graph.block_mut(block).body = kept;
    }
    let mut folded_count = replaced.len();
    if !replaced.is_empty() {
        for &block in layout.iter() {
            let data = graph.block(block).clone();
            for node in data.phis.iter().chain(data.control.iter()) {
                let inputs: SmallVec<[NodeId; 3]> = graph
                    .node(*node)
                    .inputs
                    .iter()
                    .map(|&input| resolve(&replaced, input))
                    .collect();
                graph.node_mut(*node).inputs = inputs;
            }
        }
        for state in &mut graph.frame_states {
            state.for_each_value_mut(|value| *value = resolve(&replaced, *value));
        }
        for node in &mut graph.nodes {
            if let Kind::VirtualObject { .. } = node.kind {
                for input in &mut node.inputs {
                    *input = resolve(&replaced, *input);
                }
            }
        }
    }
    // Branches.
    let implied = implied_by_dominating_branches(graph, layout);
    let entries: Vec<BlockId> = layout
        .iter()
        .copied()
        .filter(|&block| graph.block(block).predecessors.is_empty())
        .collect();
    let mut edges_removed = false;
    for &block in layout.iter() {
        let Some(control) = graph.block(block).control else {
            continue;
        };
        let Some(taken) = decided(graph, control).or_else(|| implied.get(&control).copied()) else {
            continue;
        };
        let Kind::Branch {
            if_true, if_false, ..
        } = graph.node(control).kind
        else {
            continue;
        };
        let (target, other) = if taken {
            (if_true, if_false)
        } else {
            (if_false, if_true)
        };
        let data = graph.node_mut(control);
        data.kind = Kind::Jump(target);
        data.inputs.clear();
        folded_count += 1;
        if other != target {
            remove_edge(graph, block, other);
            edges_removed = true;
        }
    }
    if folded_count == 0 {
        return 0;
    }
    // Blocks no entry reaches any more, a loop that lost its entering edge
    // included, leave with their edges.
    let mut removed: FxHashSet<BlockId> = FxHashSet::default();
    if edges_removed {
        let mut reachable: FxHashSet<BlockId> = FxHashSet::default();
        let mut work = entries.clone();
        while let Some(block) = work.pop() {
            if reachable.insert(block) {
                work.extend(successors(graph, block));
            }
        }
        removed = layout
            .iter()
            .copied()
            .filter(|block| !reachable.contains(block))
            .collect();
        for &block in &removed {
            for successor in successors(graph, block) {
                remove_edge(graph, block, successor);
            }
        }
        for &block in &removed {
            let data = graph.block_mut(block);
            data.phis.clear();
            data.body.clear();
            data.control = None;
            data.predecessors.clear();
        }
        layout.retain(|block| !removed.contains(block));
    }
    // A header keeps its loop while a back edge still enters it.
    loop_headers.retain(|header| {
        if removed.contains(&header.block) {
            return false;
        }
        let looping = graph
            .block(header.block)
            .predecessors
            .iter()
            .any(|&predecessor| {
                graph.block(predecessor).control.is_some_and(|control| {
                    matches!(graph.node(control).kind, Kind::JumpLoop(target) if target == header.block)
                })
            });
        if !looping {
            graph.block_mut(header.block).is_loop = false;
        }
        looping
    });
    super::builder::remove_trivial_phis(graph, layout);
    for header in loop_headers.iter_mut() {
        let phis = &graph.block(header.block).phis;
        header.phis.retain(|(_, phi)| phis.contains(phi));
    }
    folded_count
}

#[cfg(test)]
#[path = "fold_tests.rs"]
mod tests;
