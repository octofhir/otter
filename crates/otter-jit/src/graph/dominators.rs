//! Immediate dominators of the blocks of a graph.
//!
//! A block dominates another when every path from an entry to the second
//! passes through the first. The graph may have several entries (the
//! function entry and OSR entries, every block without predecessors); they
//! hang off one virtual root, so two blocks reached from different entries
//! only share that root.
//!
//! # Contents
//! - [`Dominators`] — immediate dominators over reverse postorder, computed
//!   by iterating the intersect-of-predecessors equation to a fixed point.
//! - [`successors`] — a block's control successors.
//!
//! # Invariants
//! - Unreachable blocks have no dominator and dominate nothing.
//! - [`Dominators::dominates`] is reflexive.
//!
//! # See also
//! - [`super::licm`] — reads which loop blocks run every iteration.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::ir::{BlockId, Graph, Kind};

/// The control successors of `block`.
#[must_use]
pub(crate) fn successors(graph: &Graph, block: BlockId) -> SmallVec<[BlockId; 2]> {
    let Some(control) = graph.block(block).control else {
        return SmallVec::new();
    };
    match graph.node(control).kind {
        Kind::Jump(target) | Kind::JumpLoop(target) => smallvec::smallvec![target],
        Kind::Branch {
            if_true, if_false, ..
        } => smallvec::smallvec![if_true, if_false],
        _ => SmallVec::new(),
    }
}

/// Immediate dominators.
pub(crate) struct Dominators {
    /// Reverse-postorder number of each reachable block; the virtual root is
    /// number zero.
    order: FxHashMap<BlockId, u32>,
    /// Immediate dominator by reverse-postorder number; the root's is
    /// itself, an entry's is the root.
    idom: Vec<u32>,
}

impl Dominators {
    /// Compute the dominators of every block reachable from an entry.
    #[must_use]
    pub(crate) fn compute(graph: &Graph, blocks: &[BlockId]) -> Self {
        let entries: Vec<BlockId> = blocks
            .iter()
            .copied()
            .filter(|&block| graph.block(block).predecessors.is_empty())
            .collect();
        // Iterative depth-first postorder from every entry.
        let mut postorder: Vec<BlockId> = Vec::new();
        let mut visited: FxHashMap<BlockId, ()> = FxHashMap::default();
        for &entry in &entries {
            if visited.insert(entry, ()).is_some() {
                continue;
            }
            let mut stack: Vec<(BlockId, SmallVec<[BlockId; 2]>)> =
                vec![(entry, successors(graph, entry))];
            while let Some((block, pending)) = stack.last_mut() {
                if let Some(next) = pending.pop() {
                    if visited.insert(next, ()).is_none() {
                        let next_successors = successors(graph, next);
                        stack.push((next, next_successors));
                    }
                } else {
                    postorder.push(*block);
                    stack.pop();
                }
            }
        }
        let mut order = FxHashMap::default();
        for (index, &block) in postorder.iter().rev().enumerate() {
            order.insert(block, index as u32 + 1);
        }
        let count = postorder.len() + 1;
        let mut idom = vec![u32::MAX; count];
        idom[0] = 0;
        for &entry in &entries {
            idom[order[&entry] as usize] = 0;
        }
        let intersect = |idom: &[u32], mut a: u32, mut b: u32| -> u32 {
            while a != b {
                while a > b {
                    a = idom[a as usize];
                }
                while b > a {
                    b = idom[b as usize];
                }
            }
            a
        };
        let mut changed = true;
        while changed {
            changed = false;
            for &block in postorder.iter().rev() {
                let number = order[&block];
                if idom[number as usize] == 0 && entries.contains(&block) {
                    continue;
                }
                let mut new_idom = u32::MAX;
                for predecessor in &graph.block(block).predecessors {
                    let Some(&p) = order.get(predecessor) else {
                        continue;
                    };
                    if idom[p as usize] == u32::MAX {
                        continue;
                    }
                    new_idom = if new_idom == u32::MAX {
                        p
                    } else {
                        intersect(&idom, p, new_idom)
                    };
                }
                if new_idom != u32::MAX && idom[number as usize] != new_idom {
                    idom[number as usize] = new_idom;
                    changed = true;
                }
            }
        }
        Self { order, idom }
    }

    /// Whether `a` dominates `b`; false when either is unreachable.
    #[must_use]
    pub(crate) fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        let (Some(&a), Some(&b)) = (self.order.get(&a), self.order.get(&b)) else {
            return false;
        };
        let mut current = b;
        loop {
            if current == a {
                return true;
            }
            if current == 0 || self.idom[current as usize] == u32::MAX {
                return false;
            }
            let next = self.idom[current as usize];
            if next >= current {
                return false;
            }
            current = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::ir::{BranchKind, Repr};

    fn jump(graph: &mut Graph, from: BlockId, kind: Kind) {
        let node = graph.add_node(kind, &[], Repr::None);
        graph.node_mut(node).block = Some(from);
        graph.block_mut(from).control = Some(node);
    }

    #[test]
    fn diamond_and_loop_dominance() {
        // b0 -> b1; b1 -> b2 | b3; b2 -> b4; b3 -> b4; b4 -> b1 (loop) ; b5 OSR -> b4
        let mut graph = Graph::default();
        let b: Vec<BlockId> = (0..6).map(|_| graph.new_block()).collect();
        let cond = graph.add_node(Kind::ConstInt32(0), &[], Repr::Int32);
        jump(&mut graph, b[0], Kind::Jump(b[1]));
        let branch = graph.add_node(
            Kind::Branch {
                kind: BranchKind::WordEqual(0),
                if_true: b[2],
                if_false: b[3],
            },
            &[cond],
            Repr::None,
        );
        graph.node_mut(branch).block = Some(b[1]);
        graph.block_mut(b[1]).control = Some(branch);
        jump(&mut graph, b[2], Kind::Jump(b[4]));
        jump(&mut graph, b[3], Kind::Jump(b[4]));
        jump(&mut graph, b[4], Kind::JumpLoop(b[1]));
        jump(&mut graph, b[5], Kind::Jump(b[4]));
        graph.block_mut(b[1]).predecessors = vec![b[0], b[4]];
        graph.block_mut(b[2]).predecessors = vec![b[1]];
        graph.block_mut(b[3]).predecessors = vec![b[1]];
        graph.block_mut(b[4]).predecessors = vec![b[2], b[3], b[5]];
        let dominators = Dominators::compute(&graph, &b);
        // b1 is also reached from the second entry through b4.
        assert!(!dominators.dominates(b[0], b[1]));
        assert!(dominators.dominates(b[0], b[0]));
        assert!(dominators.dominates(b[1], b[2]));
        assert!(dominators.dominates(b[1], b[3]));
        assert!(dominators.dominates(b[1], b[1]));
        assert!(!dominators.dominates(b[2], b[4]));
        // The OSR entry reaches b4 without b1.
        assert!(!dominators.dominates(b[1], b[4]));
        assert!(!dominators.dominates(b[5], b[1]));
    }
}
