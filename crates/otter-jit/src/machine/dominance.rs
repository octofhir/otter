//! Dominator trees for the optimizer's control-flow graphs.
//!
//! One construction serves every graph the optimizing tier analyzes: scalar
//! HIR blocks (`numeric::partial_escape`) and Machine blocks (`gvn`, `licm`).
//! Callers describe the graph by block count, entry, and a successor walk.
//!
//! # Contents
//! - [`Dominance::compute`]: reachability, reverse postorder, immediate
//!   dominators by the Cooper–Harvey–Kennedy iterative intersection over
//!   reverse-postorder numbers, sorted dominator-tree children, and pre/post
//!   numbering of the tree.
//! - [`Dominance::dominates`]: constant-time ancestor query on that numbering.
//!
//! # Invariants
//! - Only blocks reachable from the entry have an immediate dominator; the
//!   entry has none.
//! - `dominates(a, b)` is reflexive and is false whenever `a` or `b` is
//!   unreachable from the entry.
//! - Children lists are sorted by block index, so tree walks are
//!   deterministic.
//! - Construction is linear in blocks plus edges per fixpoint round and never
//!   materializes per-block dominator sets.
//!
//! # See also
//! - `super::gvn` walks the tree scope by scope.
//! - `super::licm` discovers natural loops from back edges whose target
//!   dominates their source.

/// Immediate-dominator tree of one control-flow graph.
#[derive(Debug, Clone)]
pub(super) struct Dominance {
    children: Vec<Vec<usize>>,
    preorder: Vec<u32>,
    postorder: Vec<u32>,
    reachable: Vec<bool>,
}

impl Dominance {
    /// Builds the dominator tree of the graph with `block_count` blocks rooted
    /// at `entry`, whose edges `successors` enumerates per block.
    pub(super) fn compute<I>(
        block_count: usize,
        entry: usize,
        mut successors: impl FnMut(usize) -> I,
    ) -> Self
    where
        I: IntoIterator<Item = usize>,
    {
        let edges = (0..block_count)
            .map(|block| successors(block).into_iter().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let mut predecessors = vec![Vec::new(); block_count];
        for (block, targets) in edges.iter().enumerate() {
            for &target in targets {
                predecessors[target].push(block);
            }
        }

        // Reverse postorder of the blocks reachable from the entry.
        let mut reachable = vec![false; block_count];
        let mut postorder_blocks = Vec::with_capacity(block_count);
        if entry < block_count {
            let mut stack = vec![(entry, 0usize)];
            reachable[entry] = true;
            while let Some((block, next_edge)) = stack.last_mut() {
                let block = *block;
                if let Some(&successor) = edges[block].get(*next_edge) {
                    *next_edge += 1;
                    if !reachable[successor] {
                        reachable[successor] = true;
                        stack.push((successor, 0));
                    }
                } else {
                    postorder_blocks.push(block);
                    stack.pop();
                }
            }
        }
        let mut rpo_number = vec![u32::MAX; block_count];
        for (number, &block) in postorder_blocks.iter().rev().enumerate() {
            rpo_number[block] = number as u32;
        }

        let mut idom = vec![None; block_count];
        if entry < block_count {
            idom[entry] = Some(entry);
        }
        let intersect = |idom: &[Option<usize>], mut left: usize, mut right: usize| {
            while left != right {
                while rpo_number[left] > rpo_number[right] {
                    left = idom[left].expect("processed block has a dominator");
                }
                while rpo_number[right] > rpo_number[left] {
                    right = idom[right].expect("processed block has a dominator");
                }
            }
            left
        };
        loop {
            let mut changed = false;
            for &block in postorder_blocks.iter().rev() {
                if block == entry {
                    continue;
                }
                let mut next = None;
                for &predecessor in &predecessors[block] {
                    if idom[predecessor].is_none() {
                        continue;
                    }
                    next = Some(match next {
                        None => predecessor,
                        Some(current) => intersect(&idom, predecessor, current),
                    });
                }
                if next.is_some() && idom[block] != next {
                    idom[block] = next;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        if entry < block_count {
            idom[entry] = None;
        }

        let mut children = vec![Vec::new(); block_count];
        for (block, parent) in idom.iter().enumerate() {
            if let Some(parent) = *parent {
                children[parent].push(block);
            }
        }
        let mut preorder = vec![u32::MAX; block_count];
        let mut postorder = vec![u32::MAX; block_count];
        if entry < block_count {
            let mut clock = 0u32;
            let mut stack = vec![(entry, 0usize)];
            preorder[entry] = clock;
            clock += 1;
            while let Some((block, next_child)) = stack.last_mut() {
                let block = *block;
                if let Some(&child) = children[block].get(*next_child) {
                    *next_child += 1;
                    preorder[child] = clock;
                    clock += 1;
                    stack.push((child, 0));
                } else {
                    postorder[block] = clock;
                    clock += 1;
                    stack.pop();
                }
            }
        }
        Self {
            children,
            preorder,
            postorder,
            reachable,
        }
    }

    /// Whether every path from the entry to `block` passes through
    /// `dominator`.
    pub(super) fn dominates(&self, dominator: usize, block: usize) -> bool {
        self.reachable[dominator]
            && self.reachable[block]
            && self.preorder[dominator] <= self.preorder[block]
            && self.postorder[block] <= self.postorder[dominator]
    }

    pub(super) fn is_reachable(&self, block: usize) -> bool {
        self.reachable[block]
    }

    /// Blocks immediately dominated by `block`, in ascending index order.
    pub(super) fn children(&self, block: usize) -> &[usize] {
        &self.children[block]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[&[usize]]) -> Dominance {
        Dominance::compute(edges.len(), 0, |block| edges[block].iter().copied())
    }

    fn immediate_dominator(dominance: &Dominance, block: usize) -> Option<usize> {
        (0..dominance.children.len()).find(|&parent| dominance.children(parent).contains(&block))
    }

    #[test]
    fn diamond_join_is_dominated_by_the_split() {
        let dominance = graph(&[&[1, 2], &[3], &[3], &[]]);
        assert_eq!(immediate_dominator(&dominance, 3), Some(0));
        assert!(dominance.dominates(0, 3));
        assert!(!dominance.dominates(1, 3));
        assert!(dominance.dominates(3, 3));
        assert_eq!(dominance.children(0), &[1, 2, 3]);
    }

    #[test]
    fn loop_header_dominates_its_latch_and_exit() {
        // 0 -> 1 (header) -> 2 (body) -> 1, 1 -> 3 (exit)
        let dominance = graph(&[&[1], &[2, 3], &[1], &[]]);
        assert!(dominance.dominates(1, 2));
        assert!(dominance.dominates(1, 3));
        assert!(!dominance.dominates(2, 1));
        assert_eq!(immediate_dominator(&dominance, 1), Some(0));
    }

    #[test]
    fn irreducible_entries_meet_at_the_common_dominator() {
        // 0 -> {1, 2}; 1 <-> 2; both -> 3
        let dominance = graph(&[&[1, 2], &[2, 3], &[1, 3], &[]]);
        assert_eq!(immediate_dominator(&dominance, 1), Some(0));
        assert_eq!(immediate_dominator(&dominance, 2), Some(0));
        assert_eq!(immediate_dominator(&dominance, 3), Some(0));
    }

    #[test]
    fn unreachable_blocks_dominate_nothing() {
        let dominance = graph(&[&[1], &[], &[1]]);
        assert!(!dominance.is_reachable(2));
        assert!(!dominance.dominates(2, 1));
        assert!(!dominance.dominates(0, 2));
        assert_eq!(immediate_dominator(&dominance, 1), Some(0));
    }
}
