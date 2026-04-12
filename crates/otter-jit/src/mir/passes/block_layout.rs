//! Block layout pass (branch shaping).
//!
//! Reorders blocks so that:
//! 1. Entry block stays first.
//! 2. Fallthrough targets are placed immediately after their predecessor
//!    (reducing taken branches on the hot path).
//! 3. Deopt/bailout blocks are placed at the end (cold code).
//!
//! Spec: Phase 1.5 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashSet;

use crate::mir::graph::{BlockId, MirGraph};
use crate::mir::nodes::MirOp;

/// Run block layout optimization on the MIR graph.
pub fn run(graph: &mut MirGraph) {
    if graph.blocks.len() <= 2 {
        return; // Nothing to reorder.
    }

    // Classify blocks: deopt blocks contain only a Deopt terminator.
    let deopt_blocks: HashSet<BlockId> = graph
        .blocks
        .iter()
        .filter(|b| is_deopt_block(b))
        .map(|b| b.id)
        .collect();

    // Build a greedy layout: DFS from entry, preferring fallthrough.
    let mut ordered: Vec<BlockId> = Vec::with_capacity(graph.blocks.len());
    let mut visited: HashSet<BlockId> = HashSet::new();
    let mut deferred_deopt: Vec<BlockId> = Vec::new();

    // Start with entry block.
    let mut worklist = vec![graph.entry_block];

    while let Some(block_id) = worklist.pop() {
        if !visited.insert(block_id) {
            continue;
        }

        if deopt_blocks.contains(&block_id) {
            deferred_deopt.push(block_id);
            continue;
        }

        ordered.push(block_id);

        // Get successors, prefer fallthrough (first successor = likely path).
        let block = &graph.blocks[block_id.0 as usize];
        let succs: Vec<BlockId> = block.successors.clone();

        // Push in reverse so the first successor is processed first (DFS preorder).
        for &succ in succs.iter().rev() {
            if !visited.contains(&succ) {
                worklist.push(succ);
            }
        }
    }

    // Append deopt blocks at the end (cold code).
    ordered.extend(deferred_deopt);

    // Add any unreachable blocks we missed (shouldn't happen, but be safe).
    for block in &graph.blocks {
        if !visited.contains(&block.id) {
            ordered.push(block.id);
        }
    }

    // Reorder blocks in-place.
    if ordered.len() != graph.blocks.len() {
        return; // Safety: don't reorder if counts don't match.
    }

    // Build index map: new_position -> old_block_id.
    let mut new_blocks = Vec::with_capacity(graph.blocks.len());
    for &block_id in &ordered {
        // Clone the block from its current position.
        let block = graph.blocks[block_id.0 as usize].clone();
        new_blocks.push(block);
    }
    graph.blocks = new_blocks;

    // Update entry_block (always first after layout).
    // The entry block should already be first, but verify.
    if graph.blocks[0].id != graph.entry_block {
        // Find entry and swap to front.
        if let Some(pos) = graph.blocks.iter().position(|b| b.id == graph.entry_block) {
            graph.blocks.swap(0, pos);
        }
    }
}

/// A block is a "deopt block" if it contains only a Deopt terminator
/// (or possibly a few setup instructions + Deopt).
fn is_deopt_block(block: &crate::mir::graph::BasicBlock) -> bool {
    block
        .instrs
        .last()
        .is_some_and(|i| matches!(i.op, MirOp::Deopt(_)))
}
