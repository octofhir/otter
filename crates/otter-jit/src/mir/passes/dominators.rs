//! Dominator tree computation for the MIR CFG.
//!
//! Uses the simple iterative algorithm (Cooper, Harvey, Kennedy 2001).
//! Sufficient for our graph sizes; SM's Semi-NCA is faster for huge functions
//! but adds complexity we don't need yet.
//!
//! The dominator tree enables cross-block guard elimination: if a GuardInt32
//! in block A dominates block B, all GuardInt32 checks on the same value
//! in B can be eliminated.
//!
//! Spec: Phase 4.4 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashMap;

use crate::mir::graph::{BlockId, MirGraph};

/// Dominator tree for the MIR CFG.
#[derive(Debug, Clone)]
pub struct DominatorTree {
    /// Immediate dominator for each block. Entry block has no idom (maps to itself).
    idom: HashMap<BlockId, BlockId>,
    /// Reverse post-order numbering (block → RPO index).
    rpo_order: Vec<BlockId>,
}

impl DominatorTree {
    /// Compute the dominator tree for a MIR graph.
    ///
    /// Requires that edges have been computed (`graph.recompute_edges()`).
    #[must_use]
    pub fn compute(graph: &MirGraph) -> Self {
        let rpo_order = reverse_postorder(graph);
        let mut rpo_index: HashMap<BlockId, usize> = HashMap::new();
        for (idx, &block) in rpo_order.iter().enumerate() {
            rpo_index.insert(block, idx);
        }

        let entry = graph.entry_block;
        let mut idom: HashMap<BlockId, BlockId> = HashMap::new();
        idom.insert(entry, entry);

        // Iterative dominator computation (Cooper et al. 2001).
        let mut changed = true;
        while changed {
            changed = false;
            for &block in &rpo_order {
                if block == entry {
                    continue;
                }
                let preds = &graph.block(block).predecessors;
                // Find first processed predecessor.
                let mut new_idom = None;
                for &pred in preds {
                    if idom.contains_key(&pred) {
                        new_idom = Some(pred);
                        break;
                    }
                }
                let Some(mut new_idom_val) = new_idom else {
                    continue;
                };

                // Intersect with remaining predecessors.
                for &pred in preds {
                    if pred == new_idom_val {
                        continue;
                    }
                    if idom.contains_key(&pred) {
                        new_idom_val = intersect(pred, new_idom_val, &idom, &rpo_index);
                    }
                }

                if idom.get(&block) != Some(&new_idom_val) {
                    idom.insert(block, new_idom_val);
                    changed = true;
                }
            }
        }

        Self { idom, rpo_order }
    }

    /// Get the immediate dominator of a block.
    #[must_use]
    pub fn idom(&self, block: BlockId) -> Option<BlockId> {
        self.idom.get(&block).copied()
    }

    /// Whether block `a` dominates block `b` (a dom b).
    ///
    /// A block dominates itself.
    #[must_use]
    pub fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        if a == b {
            return true;
        }
        let mut cursor = b;
        loop {
            match self.idom.get(&cursor) {
                Some(&idom) if idom == cursor => return false, // Reached entry.
                Some(&idom) if idom == a => return true,
                Some(&idom) => cursor = idom,
                None => return false,
            }
        }
    }

    /// Blocks in reverse post-order.
    #[must_use]
    pub fn rpo_order(&self) -> &[BlockId] {
        &self.rpo_order
    }
}

/// Intersect two dominators (walk up the tree until they meet).
fn intersect(
    mut a: BlockId,
    mut b: BlockId,
    idom: &HashMap<BlockId, BlockId>,
    rpo_index: &HashMap<BlockId, usize>,
) -> BlockId {
    while a != b {
        let a_idx = rpo_index.get(&a).copied().unwrap_or(usize::MAX);
        let b_idx = rpo_index.get(&b).copied().unwrap_or(usize::MAX);
        if a_idx > b_idx {
            a = idom.get(&a).copied().unwrap_or(a);
        } else {
            b = idom.get(&b).copied().unwrap_or(b);
        }
        // Safety: prevent infinite loop if graph is malformed.
        if a == idom.get(&a).copied().unwrap_or(a)
            && b == idom.get(&b).copied().unwrap_or(b)
            && a != b
        {
            break;
        }
    }
    a
}

/// Compute reverse post-order traversal of the CFG.
fn reverse_postorder(graph: &MirGraph) -> Vec<BlockId> {
    let mut visited = std::collections::HashSet::new();
    let mut post_order = Vec::new();

    fn dfs(
        block: BlockId,
        graph: &MirGraph,
        visited: &mut std::collections::HashSet<BlockId>,
        post_order: &mut Vec<BlockId>,
    ) {
        if !visited.insert(block) {
            return;
        }
        for &succ in &graph.block(block).successors {
            dfs(succ, graph, visited, post_order);
        }
        post_order.push(block);
    }

    dfs(graph.entry_block, graph, &mut visited, &mut post_order);
    post_order.reverse();
    post_order
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::graph::MirGraph;
    use crate::mir::nodes::MirOp;

    fn make_diamond_graph() -> MirGraph {
        //   bb0
        //  /   \
        // bb1  bb2
        //  \   /
        //   bb3
        let mut graph = MirGraph::new("diamond".into(), 0, 1, 0);
        let bb0 = graph.entry_block;
        let bb1 = graph.create_block();
        let bb2 = graph.create_block();
        let bb3 = graph.create_block();

        let cond = graph.push_instr(bb0, MirOp::True, 0);
        graph.push_instr(
            bb0,
            MirOp::Branch {
                cond,
                true_block: bb1,
                true_args: vec![],
                false_block: bb2,
                false_args: vec![],
            },
            1,
        );

        graph.push_instr(bb1, MirOp::Jump(bb3, vec![]), 2);
        graph.push_instr(bb2, MirOp::Jump(bb3, vec![]), 3);

        let undef = graph.push_instr(bb3, MirOp::Undefined, 4);
        graph.push_instr(bb3, MirOp::Return(undef), 5);

        graph.recompute_edges();
        graph
    }

    #[test]
    fn test_dominator_diamond() {
        let graph = make_diamond_graph();
        let dom = DominatorTree::compute(&graph);

        let bb0 = BlockId(0);
        let bb1 = BlockId(1);
        let bb2 = BlockId(2);
        let bb3 = BlockId(3);

        // bb0 dominates everything.
        assert!(dom.dominates(bb0, bb0));
        assert!(dom.dominates(bb0, bb1));
        assert!(dom.dominates(bb0, bb2));
        assert!(dom.dominates(bb0, bb3));

        // bb1 and bb2 don't dominate each other.
        assert!(!dom.dominates(bb1, bb2));
        assert!(!dom.dominates(bb2, bb1));

        // bb3's idom is bb0 (not bb1 or bb2, since either path reaches bb3).
        assert_eq!(dom.idom(bb3), Some(bb0));
    }

    #[test]
    fn test_dominator_linear() {
        // bb0 → bb1 → bb2
        let mut graph = MirGraph::new("linear".into(), 0, 1, 0);
        let bb0 = graph.entry_block;
        let bb1 = graph.create_block();
        let bb2 = graph.create_block();

        graph.push_instr(bb0, MirOp::Jump(bb1, vec![]), 0);
        graph.push_instr(bb1, MirOp::Jump(bb2, vec![]), 1);
        let undef = graph.push_instr(bb2, MirOp::Undefined, 2);
        graph.push_instr(bb2, MirOp::Return(undef), 3);

        graph.recompute_edges();
        let dom = DominatorTree::compute(&graph);

        assert!(dom.dominates(bb0, bb2));
        assert!(dom.dominates(bb1, bb2));
        assert!(!dom.dominates(bb2, bb1));
        assert_eq!(dom.idom(bb1), Some(bb0));
        assert_eq!(dom.idom(bb2), Some(bb1));
    }

    #[test]
    fn test_rpo_order() {
        let graph = make_diamond_graph();
        let dom = DominatorTree::compute(&graph);
        let rpo = dom.rpo_order();

        // Entry block should be first.
        assert_eq!(rpo[0], BlockId(0));
        // Merge block should be last (deepest in post-order).
        assert_eq!(*rpo.last().unwrap(), BlockId(3));
    }
}
