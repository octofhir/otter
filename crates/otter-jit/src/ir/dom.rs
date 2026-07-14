//! Deterministic dominance analyses over a verified bytecode control-flow graph.
//!
//! # Contents
//! - [`DominatorTree`] — immediate dominators and reverse-postorder traversal.
//! - [`DominanceFrontier`] — Cytron dominance-frontier sets.
//! - [`DomError`] — precise, pure verification failures.
//!
//! # Invariants
//! - Full-edge dominance includes every normal and exception control transfer;
//!   normal-edge dominance uses the same algorithms over normal flow only.
//! - Block-indexed storage is dense and deterministic; frontier sets are sorted
//!   and duplicate-free.
//! - Normal-edge analysis is a forest rooted at entry and exception handlers;
//!   its conceptual virtual root is never exposed as a [`BlockId`].
//! - Analysis construction reads immutable CFG data and has no runtime effect.
//!
//! # See also
//! - [`crate::ir::cfg::ControlFlowGraph`]

use smallvec::SmallVec;

use super::cfg::{Block, BlockId, ControlFlowGraph};

/// Immediate dominators and reverse-postorder for one verified CFG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DominatorTree {
    /// Immediate dominator by dense block id; virtual-root children store `None`.
    idom: Box<[Option<BlockId>]>,
    /// Reverse-postorder over the selected edge set, excluding the virtual root.
    rpo: Box<[BlockId]>,
    edges: EdgeSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeSelection {
    Full,
    Normal,
}

/// Sorted dominance-frontier sets indexed by dense block id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DominanceFrontier {
    df: Box<[SmallVec<[BlockId; 4]>]>,
}

/// Failure to verify a dominator tree or dominance frontier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomError {
    /// Immediate-dominator storage does not cover exactly the CFG blocks.
    ImmediateDominatorCountMismatch {
        /// Number of CFG blocks.
        expected: usize,
        /// Number of stored immediate dominators.
        actual: usize,
    },
    /// Stored reverse-postorder differs from deterministic full-edge DFS order.
    ReversePostorderMismatch {
        /// Reverse-postorder recomputed from the CFG.
        expected: Box<[BlockId]>,
        /// Stored reverse-postorder.
        actual: Box<[BlockId]>,
    },
    /// Entry incorrectly has an exposed immediate dominator.
    EntryHasImmediateDominator {
        /// Invalid immediate dominator of entry.
        dominator: BlockId,
    },
    /// A non-entry block has no immediate dominator.
    MissingImmediateDominator {
        /// Block missing its immediate dominator.
        block: BlockId,
    },
    /// An immediate dominator lies outside the dense CFG block range.
    ImmediateDominatorOutOfRange {
        /// Block owning the invalid relation.
        block: BlockId,
        /// Out-of-range immediate dominator.
        dominator: BlockId,
    },
    /// A non-entry block names itself as its immediate dominator.
    ImmediateDominatorIsSelf {
        /// Self-dominating block.
        block: BlockId,
    },
    /// Following immediate dominators encounters a cycle before entry.
    ImmediateDominatorCycle {
        /// Block whose parent chain contains the cycle.
        block: BlockId,
    },
    /// An immediate dominator does not occur before its child in reverse-postorder.
    ImmediateDominatorNotBeforeBlock {
        /// Dominated block.
        block: BlockId,
        /// Immediate dominator ordered too late.
        dominator: BlockId,
    },
    /// An immediate dominator fails to dominate one of the block's predecessors.
    ImmediateDominatorDoesNotDominatePredecessor {
        /// Block whose relation is unsound.
        block: BlockId,
        /// Stored immediate dominator.
        dominator: BlockId,
        /// Predecessor not dominated by the stored immediate dominator.
        predecessor: BlockId,
    },
    /// Stored immediate dominators are not the Cooper-Harvey-Kennedy fixpoint.
    ImmediateDominatorFixpointMismatch {
        /// Block whose immediate dominator differs.
        block: BlockId,
        /// Immediate dominator recomputed by the CHK fixpoint.
        expected: BlockId,
        /// Stored immediate dominator.
        actual: BlockId,
    },
    /// A block is attached to the virtual root unexpectedly.
    UnexpectedDominatorRoot {
        /// Block whose expected immediate dominator is a real block.
        block: BlockId,
        /// Recomputed immediate dominator.
        expected: BlockId,
    },
    /// A block is attached below a real block instead of the virtual root.
    MissingDominatorRoot {
        /// Block that should be a dominance-forest root.
        block: BlockId,
        /// Unexpected stored immediate dominator.
        actual: BlockId,
    },
    /// Frontier storage does not cover exactly the CFG blocks.
    DominanceFrontierCountMismatch {
        /// Number of CFG blocks.
        expected: usize,
        /// Number of stored frontier sets.
        actual: usize,
    },
    /// A frontier contains a block outside the dense CFG block range.
    DominanceFrontierBlockOutOfRange {
        /// Block owning the frontier set.
        owner: BlockId,
        /// Out-of-range frontier member.
        block: BlockId,
    },
    /// A frontier set is not strictly sorted and duplicate-free.
    DominanceFrontierNotCanonical {
        /// Block owning the non-canonical frontier set.
        block: BlockId,
    },
    /// A stored frontier differs from the direct dominance definition.
    DominanceFrontierDefinitionMismatch {
        /// Block owning the mismatched frontier set.
        block: BlockId,
        /// Frontier recomputed directly from the definition.
        expected: Box<[BlockId]>,
        /// Stored frontier set.
        actual: Box<[BlockId]>,
    },
}

impl std::fmt::Display for DomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid dominance analysis: {self:?}")
    }
}

impl std::error::Error for DomError {}

impl DominatorTree {
    /// Compute immediate dominators with the Cooper-Harvey-Kennedy algorithm.
    #[must_use]
    pub fn compute(cfg: &ControlFlowGraph) -> Self {
        Self::compute_with_edges(cfg, EdgeSelection::Full)
    }

    /// Compute normal-flow dominators with the same CHK implementation.
    ///
    /// Entry and every exception-handler target are children of a conceptual
    /// virtual root, so handler-only regions form independent dominance trees.
    #[must_use]
    pub fn compute_normal(cfg: &ControlFlowGraph) -> Self {
        Self::compute_with_edges(cfg, EdgeSelection::Normal)
    }

    fn compute_with_edges(cfg: &ControlFlowGraph, edges: EdgeSelection) -> Self {
        let rpo = reverse_postorder(cfg, edges);
        let idom = compute_idoms(cfg, &rpo, edges);
        Self {
            idom: idom.into_boxed_slice(),
            rpo: rpo.into_boxed_slice(),
            edges,
        }
    }

    /// Return `block`'s immediate dominator, or `None` for entry.
    #[must_use]
    pub fn immediate_dominator(&self, block: BlockId) -> Option<BlockId> {
        self.idom[block.0 as usize]
    }

    /// Return whether `a` reflexively dominates `b`.
    #[must_use]
    pub fn dominates(&self, a: BlockId, mut b: BlockId) -> bool {
        loop {
            if a == b {
                return true;
            }
            let Some(parent) = self.immediate_dominator(b) else {
                return false;
            };
            b = parent;
        }
    }

    /// Return whether `a` dominates `b` and the blocks differ.
    #[must_use]
    pub fn strictly_dominates(&self, a: BlockId, b: BlockId) -> bool {
        a != b && self.dominates(a, b)
    }

    /// Return deterministic reverse-postorder over the selected edge set.
    #[must_use]
    pub fn reverse_postorder(&self) -> &[BlockId] {
        &self.rpo
    }

    pub(crate) fn includes_exception_edges(&self) -> bool {
        self.edges == EdgeSelection::Full
    }

    /// Verify tree shape, CHK fixpoint, ordering, and predecessor soundness.
    pub fn verify(&self, cfg: &ControlFlowGraph) -> Result<(), DomError> {
        let block_count = cfg.blocks.len();
        if self.idom.len() != block_count {
            return Err(DomError::ImmediateDominatorCountMismatch {
                expected: block_count,
                actual: self.idom.len(),
            });
        }

        let expected_rpo = reverse_postorder(cfg, self.edges);
        if self.rpo.as_ref() != expected_rpo {
            return Err(DomError::ReversePostorderMismatch {
                expected: expected_rpo.into_boxed_slice(),
                actual: self.rpo.clone(),
            });
        }

        let entry_index = cfg.entry.0 as usize;
        if let Some(dominator) = self.idom[entry_index] {
            return Err(DomError::EntryHasImmediateDominator { dominator });
        }

        for index in 0..block_count {
            let block = BlockId(index as u32);
            if let Some(dominator) = self.idom[index] {
                if dominator.0 as usize >= block_count {
                    return Err(DomError::ImmediateDominatorOutOfRange { block, dominator });
                }
                if dominator == block {
                    return Err(DomError::ImmediateDominatorIsSelf { block });
                }
            }
        }

        for start in 0..block_count {
            let mut seen = vec![false; block_count];
            let mut block = BlockId(start as u32);
            loop {
                let index = block.0 as usize;
                if std::mem::replace(&mut seen[index], true) {
                    return Err(DomError::ImmediateDominatorCycle {
                        block: BlockId(start as u32),
                    });
                }
                let Some(parent) = self.idom[index] else {
                    break;
                };
                block = parent;
            }
        }

        let mut rpo_position = vec![0; block_count];
        for (position, block) in self.rpo.iter().copied().enumerate() {
            rpo_position[block.0 as usize] = position;
        }
        for index in 0..block_count {
            let block = BlockId(index as u32);
            if let Some(dominator) = self.idom[index]
                && rpo_position[dominator.0 as usize] >= rpo_position[index]
            {
                return Err(DomError::ImmediateDominatorNotBeforeBlock { block, dominator });
            }
        }

        for block in self.rpo.iter().copied() {
            let Some(dominator) = self.idom[block.0 as usize] else {
                continue;
            };
            for predecessor in predecessors(cfg, block, self.edges) {
                if !self.dominates(dominator, predecessor) {
                    return Err(DomError::ImmediateDominatorDoesNotDominatePredecessor {
                        block,
                        dominator,
                        predecessor,
                    });
                }
            }
        }

        let expected_idom = compute_idoms(cfg, &self.rpo, self.edges);
        for block in self.rpo.iter().copied() {
            match (expected_idom[block.0 as usize], self.idom[block.0 as usize]) {
                (Some(expected), Some(actual)) if actual != expected => {
                    return Err(DomError::ImmediateDominatorFixpointMismatch {
                        block,
                        expected,
                        actual,
                    });
                }
                (Some(expected), None) => {
                    return Err(DomError::UnexpectedDominatorRoot { block, expected });
                }
                (None, Some(actual)) => {
                    return Err(DomError::MissingDominatorRoot { block, actual });
                }
                _ => {}
            }
        }

        Ok(())
    }
}

impl DominanceFrontier {
    /// Compute dominance frontiers with the Cytron et al. runner algorithm.
    #[must_use]
    pub fn compute(cfg: &ControlFlowGraph, dom: &DominatorTree) -> Self {
        let mut df = vec![SmallVec::<[BlockId; 4]>::new(); cfg.blocks.len()];
        for block in &cfg.blocks {
            let block_predecessors = predecessors(cfg, block.id, dom.edges);
            let virtual_predecessor = is_analysis_root(cfg, block.id, dom.edges);
            let is_join = block_predecessors.len() + usize::from(virtual_predecessor) >= 2;
            if !is_join {
                continue;
            }
            let stop = dom.immediate_dominator(block.id);
            for predecessor in block_predecessors {
                let mut runner = Some(predecessor);
                while runner != stop {
                    let Some(current) = runner else {
                        break;
                    };
                    df[current.0 as usize].push(block.id);
                    runner = dom.immediate_dominator(current);
                }
            }
        }
        for frontier in &mut df {
            frontier.sort_unstable();
            frontier.dedup();
        }
        Self {
            df: df.into_boxed_slice(),
        }
    }

    /// Return the sorted, duplicate-free dominance frontier of `block`.
    #[must_use]
    pub fn frontier(&self, block: BlockId) -> &[BlockId] {
        &self.df[block.0 as usize]
    }

    /// Verify frontier bounds, canonical order, and the direct DF definition.
    pub fn verify(&self, cfg: &ControlFlowGraph, dom: &DominatorTree) -> Result<(), DomError> {
        let block_count = cfg.blocks.len();
        if self.df.len() != block_count {
            return Err(DomError::DominanceFrontierCountMismatch {
                expected: block_count,
                actual: self.df.len(),
            });
        }

        for (index, frontier) in self.df.iter().enumerate() {
            let owner = BlockId(index as u32);
            for &block in frontier {
                if block.0 as usize >= block_count {
                    return Err(DomError::DominanceFrontierBlockOutOfRange { owner, block });
                }
            }
            if !frontier.windows(2).all(|pair| pair[0] < pair[1]) {
                return Err(DomError::DominanceFrontierNotCanonical { block: owner });
            }
        }

        for index in 0..block_count {
            let block = BlockId(index as u32);
            let expected: Vec<_> = cfg
                .blocks
                .iter()
                .filter(|candidate| {
                    predecessors(cfg, candidate.id, dom.edges)
                        .into_iter()
                        .any(|predecessor| dom.dominates(block, predecessor))
                        && !dom.strictly_dominates(block, candidate.id)
                })
                .map(|candidate| candidate.id)
                .collect();
            if self.df[index].as_slice() != expected {
                return Err(DomError::DominanceFrontierDefinitionMismatch {
                    block,
                    expected: expected.into_boxed_slice(),
                    actual: self.df[index].clone().into_vec().into_boxed_slice(),
                });
            }
        }

        Ok(())
    }
}

fn reverse_postorder(cfg: &ControlFlowGraph, edges: EdgeSelection) -> Vec<BlockId> {
    let successors: Vec<_> = cfg
        .blocks
        .iter()
        .map(|block| successors(block, edges))
        .collect();
    let mut visited = vec![false; cfg.blocks.len()];
    let mut postorder = Vec::with_capacity(cfg.blocks.len());
    for root in analysis_roots(cfg, edges) {
        if std::mem::replace(&mut visited[root.0 as usize], true) {
            continue;
        }
        let mut stack = vec![(root, 0_usize)];
        while let Some((block, successor_index)) = stack.last_mut() {
            let block_index = block.0 as usize;
            if let Some(&successor) = successors[block_index].get(*successor_index) {
                *successor_index += 1;
                let successor_index = successor.0 as usize;
                if !std::mem::replace(&mut visited[successor_index], true) {
                    stack.push((successor, 0));
                }
            } else {
                postorder.push(*block);
                stack.pop();
            }
        }
    }

    postorder.reverse();
    postorder
}

fn successors(block: &Block, edges: EdgeSelection) -> SmallVec<[BlockId; 4]> {
    let mut successors: SmallVec<_> = match edges {
        EdgeSelection::Full => block
            .normal_succs
            .iter()
            .chain(&block.exception_succs)
            .copied()
            .collect(),
        EdgeSelection::Normal => block.normal_succs.iter().copied().collect(),
    };
    successors.sort_unstable();
    successors.dedup();
    successors
}

fn predecessors(
    cfg: &ControlFlowGraph,
    block: BlockId,
    edges: EdgeSelection,
) -> SmallVec<[BlockId; 4]> {
    cfg.blocks[block.0 as usize]
        .preds
        .iter()
        .copied()
        .filter(|predecessor| {
            edges == EdgeSelection::Full
                || cfg.blocks[predecessor.0 as usize]
                    .normal_succs
                    .contains(&block)
        })
        .collect()
}

fn analysis_roots(cfg: &ControlFlowGraph, edges: EdgeSelection) -> Vec<BlockId> {
    let mut roots = vec![cfg.entry];
    if edges == EdgeSelection::Normal {
        roots.extend(
            cfg.blocks
                .iter()
                .flat_map(|block| block.exception_succs.iter().copied()),
        );
        roots.sort_unstable();
        roots.dedup();
    }
    roots
}

fn is_analysis_root(cfg: &ControlFlowGraph, block: BlockId, edges: EdgeSelection) -> bool {
    block == cfg.entry
        || (edges == EdgeSelection::Normal
            && cfg
                .blocks
                .iter()
                .any(|candidate| candidate.exception_succs.contains(&block)))
}

fn compute_idoms(
    cfg: &ControlFlowGraph,
    rpo: &[BlockId],
    edges: EdgeSelection,
) -> Vec<Option<BlockId>> {
    let virtual_root = cfg.blocks.len();
    let mut postorder_number = vec![0; cfg.blocks.len() + 1];
    postorder_number[virtual_root] = rpo.len();
    for (rpo_index, block) in rpo.iter().copied().enumerate() {
        postorder_number[block.0 as usize] = rpo.len() - 1 - rpo_index;
    }

    let mut idom = vec![None; cfg.blocks.len() + 1];
    idom[virtual_root] = Some(virtual_root);
    let mut changed = true;
    while changed {
        changed = false;
        for &block in rpo {
            let mut block_predecessors: SmallVec<[usize; 5]> = predecessors(cfg, block, edges)
                .into_iter()
                .map(|predecessor| predecessor.0 as usize)
                .collect();
            if is_analysis_root(cfg, block, edges) {
                block_predecessors.push(virtual_root);
            }
            let mut processed = block_predecessors
                .into_iter()
                .filter(|&predecessor| idom[predecessor].is_some());
            let Some(mut new_idom) = processed.next() else {
                continue;
            };
            for predecessor in processed {
                new_idom = intersect(new_idom, predecessor, &idom, &postorder_number);
            }
            let block_index = block.0 as usize;
            if idom[block_index] != Some(new_idom) {
                idom[block_index] = Some(new_idom);
                changed = true;
            }
        }
    }
    idom.pop();
    idom.into_iter()
        .map(|parent| match parent {
            Some(parent) if parent != virtual_root => Some(BlockId(parent as u32)),
            _ => None,
        })
        .collect()
}

fn intersect(
    mut finger1: usize,
    mut finger2: usize,
    idom: &[Option<usize>],
    postorder_number: &[usize],
) -> usize {
    while finger1 != finger2 {
        while postorder_number[finger1] < postorder_number[finger2] {
            finger1 =
                idom[finger1].expect("CHK intersects only predecessors with known dominators");
        }
        while postorder_number[finger2] < postorder_number[finger1] {
            finger2 =
                idom[finger2].expect("CHK intersects only predecessors with known dominators");
        }
    }
    finger1
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{NO_HANDLER_OFFSET, Op, Operand};
    use otter_vm::{JitCompileSnapshot, jit::JitTestInstruction};

    use super::*;

    fn snapshot(instructions: Vec<(Op, Vec<Operand>)>) -> JitCompileSnapshot {
        let instructions = instructions
            .into_iter()
            .enumerate()
            .map(|(pc, (op, operands))| {
                JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands)
            })
            .collect();
        JitCompileSnapshot::without_feedback(0, 0, 8, instructions)
    }

    fn analyses(
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> (ControlFlowGraph, DominatorTree, DominanceFrontier) {
        let cfg = ControlFlowGraph::build(&snapshot(instructions)).expect("CFG builds");
        let dom = DominatorTree::compute(&cfg);
        let frontier = DominanceFrontier::compute(&cfg, &dom);
        dom.verify(&cfg).expect("dominator tree verifies");
        frontier
            .verify(&cfg, &dom)
            .expect("dominance frontier verifies");
        (cfg, dom, frontier)
    }

    #[test]
    fn straight_line_has_linear_idom_chain_and_empty_frontiers() {
        let (_cfg, dom, frontier) = analyses(vec![
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::ReturnUndefined, vec![]),
        ]);

        assert_eq!(
            dom.reverse_postorder(),
            [BlockId(0), BlockId(1), BlockId(2)]
        );
        assert_eq!(dom.immediate_dominator(BlockId(0)), None);
        assert_eq!(dom.immediate_dominator(BlockId(1)), Some(BlockId(0)));
        assert_eq!(dom.immediate_dominator(BlockId(2)), Some(BlockId(1)));
        assert!(dom.dominates(BlockId(0), BlockId(2)));
        assert!(dom.strictly_dominates(BlockId(1), BlockId(2)));
        assert!((0..3).all(|block| frontier.frontier(BlockId(block)).is_empty()));
    }

    #[test]
    fn diamond_join_has_branch_idom_and_arm_frontiers() {
        let (_cfg, dom, frontier) = analyses(vec![
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(0)],
            ),
            (Op::Nop, vec![]),
            (Op::Jump, vec![Operand::Imm32(1)]),
            (Op::Nop, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);

        assert_eq!(dom.immediate_dominator(BlockId(1)), Some(BlockId(0)));
        assert_eq!(dom.immediate_dominator(BlockId(2)), Some(BlockId(0)));
        assert_eq!(dom.immediate_dominator(BlockId(3)), Some(BlockId(0)));
        assert_eq!(frontier.frontier(BlockId(1)), [BlockId(3)]);
        assert_eq!(frontier.frontier(BlockId(2)), [BlockId(3)]);
    }

    #[test]
    fn while_loop_handles_backedge_and_latch_frontier() {
        let (_cfg, dom, frontier) = analyses(vec![
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(0)],
            ),
            (Op::Nop, vec![]),
            (Op::Jump, vec![Operand::Imm32(-3)]),
            (Op::ReturnUndefined, vec![]),
        ]);

        let header = BlockId(0);
        let latch = BlockId(1);
        assert_eq!(dom.immediate_dominator(header), None);
        assert!(dom.dominates(header, latch));
        assert_eq!(frontier.frontier(latch), [header]);
        assert_eq!(frontier.frontier(header), [header]);
    }

    #[test]
    fn nested_loops_compute_inner_and_outer_frontiers() {
        let (_cfg, dom, frontier) = analyses(vec![
            (Op::Jump, vec![Operand::Imm32(0)]),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(5), Operand::Register(0)],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(1)],
            ),
            (Op::Nop, vec![]),
            (Op::Jump, vec![Operand::Imm32(-3)]),
            (Op::Nop, vec![]),
            (Op::Jump, vec![Operand::Imm32(-6)]),
            (Op::ReturnUndefined, vec![]),
        ]);

        let outer_header = BlockId(1);
        let inner_header = BlockId(2);
        let inner_latch = BlockId(3);
        let outer_latch = BlockId(4);
        assert!(dom.dominates(outer_header, inner_header));
        assert!(dom.dominates(inner_header, inner_latch));
        assert_eq!(frontier.frontier(inner_latch), [inner_header]);
        assert_eq!(frontier.frontier(outer_latch), [outer_header]);
        assert_eq!(
            frontier.frontier(inner_header),
            [outer_header, inner_header]
        );
    }

    #[test]
    fn try_catch_uses_exception_edge_for_dominance_and_frontier() {
        let (cfg, dom, frontier) = analyses(vec![
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(4), Operand::Register(0)],
            ),
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(3),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(1),
                ],
            ),
            (
                Op::LoadGlobalOrThrow,
                vec![Operand::Register(0), Operand::ConstIndex(0)],
            ),
            (Op::LeaveTry, vec![]),
            (Op::Jump, vec![Operand::Imm32(1)]),
            (Op::Nop, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);

        let try_entry = cfg.blocks.iter().find(|block| block.start_pc == 1).unwrap();
        let try_body = cfg.blocks.iter().find(|block| block.start_pc == 2).unwrap();
        let catch = cfg.blocks.iter().find(|block| block.start_pc == 5).unwrap();
        let enclosing = dom
            .immediate_dominator(try_entry.id)
            .expect("try entry is not function entry");
        assert_eq!(try_body.exception_succs.as_slice(), &[catch.id]);
        assert_eq!(dom.immediate_dominator(catch.id), Some(enclosing));
        assert!(dom.dominates(enclosing, catch.id));
        assert!(frontier.frontier(try_body.id).contains(&catch.id));
    }

    #[test]
    fn normal_dominance_roots_handler_and_excludes_exception_frontier() {
        let cfg = ControlFlowGraph::build(&snapshot(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(3),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(1),
                ],
            ),
            (
                Op::LoadGlobalOrThrow,
                vec![Operand::Register(0), Operand::ConstIndex(0)],
            ),
            (Op::LeaveTry, vec![]),
            (Op::Jump, vec![Operand::Imm32(1)]),
            (Op::Nop, vec![]),
            (Op::Nop, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]))
        .expect("CFG builds");
        let dom = DominatorTree::compute_normal(&cfg);
        let frontier = DominanceFrontier::compute(&cfg, &dom);
        dom.verify(&cfg).expect("normal dominators verify");
        frontier
            .verify(&cfg, &dom)
            .expect("normal dominance frontier verifies");

        let try_body = cfg.blocks.iter().find(|block| block.start_pc == 1).unwrap();
        let catch = cfg.blocks.iter().find(|block| block.start_pc == 4).unwrap();
        assert_eq!(dom.immediate_dominator(catch.id), None);
        assert!(!frontier.frontier(try_body.id).contains(&catch.id));
    }

    #[test]
    fn reducible_multi_join_frontier_has_multiple_members() {
        let (_cfg, dom, frontier) = analyses(vec![
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(0)],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(3), Operand::Register(1)],
            ),
            (Op::Jump, vec![Operand::Imm32(1)]),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(1), Operand::Register(2)],
            ),
            (Op::Jump, vec![Operand::Imm32(1)]),
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::ReturnUndefined, vec![]),
        ]);

        assert_eq!(dom.immediate_dominator(BlockId(2)), Some(BlockId(1)));
        assert_eq!(frontier.frontier(BlockId(1)), [BlockId(4), BlockId(5)]);
        assert_eq!(frontier.frontier(BlockId(3)), [BlockId(4), BlockId(5)]);
    }

    #[test]
    fn verifier_rejects_wrong_immediate_dominator() {
        let (cfg, mut dom, _frontier) = analyses(vec![
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(0)],
            ),
            (Op::Nop, vec![]),
            (Op::Jump, vec![Operand::Imm32(1)]),
            (Op::Nop, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);
        dom.idom[BlockId(3).0 as usize] = Some(BlockId(1));

        assert_eq!(
            dom.verify(&cfg),
            Err(DomError::ImmediateDominatorDoesNotDominatePredecessor {
                block: BlockId(3),
                dominator: BlockId(1),
                predecessor: BlockId(2),
            })
        );
    }
}
