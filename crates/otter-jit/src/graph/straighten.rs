//! Straight-line block chains become one block.
//!
//! The builder gives every branch edge its own block, the place that edge's
//! facts and materializations live, and that block jumps on to the bytecode
//! block it reaches. When that bytecode block has no other way in, the two
//! are one straight line split in two: an arm of `cond ? a : b` is an empty
//! edge block jumping to the block that computes its value. Passes that
//! match the shape of the control flow (selects) see the arm as one block
//! only once such chains merge, and code that runs straight through needs no
//! jump between their halves.
//!
//! # Contents
//! - [`merge`] — append every block entered only by the jump that ends its
//!   one predecessor to that predecessor, over all of `layout`.
//!
//! # Invariants
//! - A block merges into its predecessor only when that predecessor ends in
//!   a `Jump` to it and it has no other predecessor, no phi and no loop: the
//!   merged block runs exactly the same nodes in the same order on every
//!   path. A loop header absorbs nothing, so no block is its own back edge.
//! - The merged block takes the successor's body and control; each block
//!   the successor's control reaches names the merged block in the same
//!   predecessor position, so phi inputs keep their edges.
//! - Blocks named outside the graph (`keep`: an OSR entry) never merge into
//!   a predecessor; they may absorb their own successors.
//!
//! # See also
//! - [`super::select`] — converts the diamonds this exposes.
//! - [`super::fold`] — drops the blocks a decided branch no longer reaches.

use rustc_hash::FxHashSet;

use super::dominators::successors;
use super::ir::{BlockId, Graph, Kind};

/// The block `block` jumps to when it may absorb it: entered only from
/// `block`, without phis, no loop header and not in `keep`. A loop header
/// absorbs nothing, so no block ever becomes its own back edge.
fn absorbable(graph: &Graph, block: BlockId, keep: &[BlockId]) -> Option<BlockId> {
    if graph.block(block).is_loop {
        return None;
    }
    let control = graph.block(block).control?;
    let Kind::Jump(next) = graph.node(control).kind else {
        return None;
    };
    let data = graph.block(next);
    (next != block
        && data.predecessors.as_slice() == [block]
        && data.phis.is_empty()
        && !data.is_loop
        && !keep.contains(&next))
    .then_some(next)
}

/// Merge every straight-line chain of `layout` into its first block.
/// Returns how many blocks left the layout.
pub(crate) fn merge(graph: &mut Graph, layout: &mut Vec<BlockId>, keep: &[BlockId]) -> usize {
    let mut removed: FxHashSet<BlockId> = FxHashSet::default();
    for &block in layout.iter() {
        if removed.contains(&block) {
            continue;
        }
        while let Some(next) = absorbable(graph, block, keep) {
            let body = std::mem::take(&mut graph.block_mut(next).body);
            for &node in &body {
                graph.node_mut(node).block = Some(block);
            }
            graph.block_mut(block).body.extend(body);
            let control = graph.block_mut(next).control.take();
            if let Some(control) = control {
                graph.node_mut(control).block = Some(block);
            }
            graph.block_mut(block).control = control;
            for successor in successors(graph, block) {
                for predecessor in &mut graph.block_mut(successor).predecessors {
                    if *predecessor == next {
                        *predecessor = block;
                    }
                }
            }
            graph.block_mut(next).predecessors.clear();
            removed.insert(next);
        }
    }
    layout.retain(|block| !removed.contains(block));
    removed.len()
}

#[cfg(test)]
#[path = "straighten_tests.rs"]
mod tests;
