//! Block layout pass — compute optimal block ordering without reordering.
//!
//! Computes a recommended block emission order so that:
//! 1. Entry block is first.
//! 2. Fallthrough targets follow their predecessor (fewer branches).
//! 3. Deopt/bailout blocks are last (cold code).
//!
//! Instead of physically reordering `graph.blocks` (which breaks BlockId indexing),
//! this pass stores the layout order as metadata that the codegen can use.
//!
//! Spec: Phase 1.5 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashSet;

use crate::mir::graph::{BlockId, MirGraph};
use crate::mir::nodes::MirOp;

/// Compute the optimal block emission order.
///
/// Returns a vector of BlockIds in the recommended emission order.
/// The graph itself is NOT modified — blocks stay at their original indices.
#[must_use]
pub fn compute_layout(graph: &MirGraph) -> Vec<BlockId> {
    if graph.blocks.len() <= 1 {
        return graph.blocks.iter().map(|b| b.id).collect();
    }

    let deopt_blocks: HashSet<BlockId> = graph
        .blocks
        .iter()
        .filter(|b| is_deopt_block(b))
        .map(|b| b.id)
        .collect();

    let mut ordered: Vec<BlockId> = Vec::with_capacity(graph.blocks.len());
    let mut visited: HashSet<BlockId> = HashSet::new();
    let mut cold: Vec<BlockId> = Vec::new();

    // DFS from entry, preferring fallthrough (first successor).
    let mut worklist = vec![graph.entry_block];
    while let Some(block_id) = worklist.pop() {
        if !visited.insert(block_id) {
            continue;
        }
        if deopt_blocks.contains(&block_id) {
            cold.push(block_id);
            continue;
        }
        ordered.push(block_id);

        let block = &graph.blocks[block_id.0 as usize];
        // Push successors in reverse so first successor is processed next (DFS).
        for &succ in block.successors.iter().rev() {
            if !visited.contains(&succ) {
                worklist.push(succ);
            }
        }
    }

    // Cold blocks at end.
    ordered.extend(cold);

    // Any unreachable blocks (shouldn't happen, safety net).
    for block in &graph.blocks {
        if !visited.contains(&block.id) {
            ordered.push(block.id);
        }
    }

    ordered
}

/// Run the block layout pass.
///
/// Stores the computed layout as the `block_order` field on the graph.
/// Does NOT reorder `graph.blocks` — the layout is advisory for codegen.
pub fn run(graph: &mut MirGraph) {
    graph.recompute_edges();
    let layout = compute_layout(graph);
    graph.set_block_order(layout);
}

fn is_deopt_block(block: &crate::mir::graph::BasicBlock) -> bool {
    block
        .instrs
        .last()
        .is_some_and(|i| matches!(i.op, MirOp::Deopt(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::graph::MirGraph;
    use crate::mir::nodes::MirOp;

    #[test]
    fn test_layout_entry_first() {
        let mut graph = MirGraph::new("test".into(), 0, 1, 0);
        let bb0 = graph.entry_block;
        let bb1 = graph.create_block();

        graph.push_instr(bb0, MirOp::Jump(bb1, vec![]), 0);
        let v = graph.push_instr(bb1, MirOp::Undefined, 1);
        graph.push_instr(bb1, MirOp::Return(v), 2);
        graph.recompute_edges();

        let layout = compute_layout(&graph);
        assert_eq!(layout[0], bb0);
        assert_eq!(layout[1], bb1);
    }

    #[test]
    fn test_layout_deopt_last() {
        use crate::mir::graph::DeoptId;

        let mut graph = MirGraph::new("test".into(), 0, 2, 0);
        let bb0 = graph.entry_block;
        let bb_deopt = graph.create_block();
        let bb_normal = graph.create_block();

        let cond = graph.push_instr(bb0, MirOp::True, 0);
        graph.push_instr(
            bb0,
            MirOp::Branch {
                cond,
                true_block: bb_normal,
                true_args: vec![],
                false_block: bb_deopt,
                false_args: vec![],
            },
            1,
        );

        let v = graph.push_instr(bb_normal, MirOp::ConstInt32(42), 2);
        graph.push_instr(bb_normal, MirOp::Return(v), 3);

        graph.push_instr(bb_deopt, MirOp::Deopt(DeoptId(0)), 4);
        graph.recompute_edges();

        let layout = compute_layout(&graph);
        // Deopt block should be last.
        assert_eq!(*layout.last().unwrap(), bb_deopt);
    }
}
