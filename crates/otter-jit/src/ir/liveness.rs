//! Deterministic backward liveness analysis over SSA values.
//!
//! # Contents
//! - [`Liveness`] — block-indexed live-in and live-out value sets.
//! - [`Liveness::compute`] — normal-edge fixpoint construction.
//! - [`Liveness::live_after_instruction`] — exact instruction-boundary live-out.
//! - [`Liveness::verify`] — independent structural and dataflow checks.
//! - [`LivenessError`] — precise verification failures.
//!
//! # Invariants
//! - Phi operands are uses on their normal predecessor edges, never uses in the
//!   block containing the phi.
//! - Exception edges do not propagate SSA-value liveness; handlers consume
//!   fresh exception-input definitions instead.
//! - Value sets use [`BTreeSet`] and block storage is dense, making analysis
//!   results deterministic.
//! - The function-entry boundary has an empty live-in set; entry head values
//!   are definitions available inside the entry block.
//! - Analysis and verification read immutable CFG and SSA data and have no
//!   runtime effect.
//!
//! # See also
//! - [`crate::ir::cfg`]
//! - [`crate::ir::dom`]
//! - [`crate::ir::ssa`]

use std::collections::BTreeSet;

use super::{
    cfg::{BlockId, ControlFlowGraph},
    dom::{DomError, DominatorTree},
    ssa::{SsaFunction, ValueDef, ValueId},
};

/// Backward SSA-value liveness indexed by dense [`BlockId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Liveness {
    live_in: Box<[BTreeSet<ValueId>]>,
    live_out: Box<[BTreeSet<ValueId>]>,
}

/// Identifies which stored liveness set failed an invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessSet {
    /// A block live-in set.
    LiveIn,
    /// A block live-out set.
    LiveOut,
}

/// Failure to verify stored SSA liveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LivenessError {
    /// Stored block-indexed vectors do not cover exactly the CFG blocks.
    BlockCountMismatch {
        /// Number of CFG blocks.
        expected: usize,
        /// Number of stored live-in sets.
        live_in: usize,
        /// Number of stored live-out sets.
        live_out: usize,
    },
    /// Verification was given a normal-edge-only dominator tree.
    NormalDominatorUsedForVerification,
    /// The supplied full-edge dominator tree is internally invalid.
    InvalidFullDominator(DomError),
    /// A stored set contains a value outside dense SSA storage.
    ValueOutOfRange {
        /// Block containing the invalid set member.
        block: BlockId,
        /// Set containing the invalid member.
        set: LivenessSet,
        /// Invalid value identity.
        value: ValueId,
        /// Number of valid SSA values.
        value_count: usize,
    },
    /// A stored live value's definition does not dominate its block.
    DefinitionDoesNotDominate {
        /// Block where the value is live.
        block: BlockId,
        /// Set containing the value.
        set: LivenessSet,
        /// Live value.
        value: ValueId,
        /// Block defining the value.
        definition: BlockId,
    },
    /// The function-entry live-in boundary is not empty.
    EntryLiveInNotEmpty {
        /// Incorrect entry live-in contents.
        values: BTreeSet<ValueId>,
    },
    /// Stored live-out differs from a direct successor-edge recomputation.
    LiveOutInconsistent {
        /// Block with inconsistent live-out data.
        block: BlockId,
        /// Directly recomputed live-out set.
        expected: BTreeSet<ValueId>,
        /// Stored live-out set.
        actual: BTreeSet<ValueId>,
    },
    /// One additional backward pass changes the stored fixpoint.
    FixpointUnstable {
        /// First block changed by the additional pass.
        block: BlockId,
        /// Recomputed live-in set.
        expected_live_in: BTreeSet<ValueId>,
        /// Stored live-in set.
        actual_live_in: BTreeSet<ValueId>,
        /// Recomputed live-out set.
        expected_live_out: BTreeSet<ValueId>,
        /// Stored live-out set.
        actual_live_out: BTreeSet<ValueId>,
    },
    /// An upward-exposed instruction use is absent from block live-in.
    UpwardExposedUseMissing {
        /// Block containing the instruction use.
        block: BlockId,
        /// Missing live value.
        value: ValueId,
    },
}

impl std::fmt::Display for LivenessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid SSA liveness: {self:?}")
    }
}

impl std::error::Error for LivenessError {}

#[derive(Debug, Clone, Default)]
struct BlockFacts {
    phi_defs: BTreeSet<ValueId>,
    instr_defs: BTreeSet<ValueId>,
    uses: BTreeSet<ValueId>,
}

impl Liveness {
    /// Compute SSA liveness to a fixpoint over normal control edges.
    #[must_use]
    pub fn compute(ssa: &SsaFunction, cfg: &ControlFlowGraph) -> Self {
        let facts = block_facts(ssa);
        let block_count = cfg.blocks.len();
        let mut result = Self {
            live_in: vec![BTreeSet::new(); block_count].into_boxed_slice(),
            live_out: vec![BTreeSet::new(); block_count].into_boxed_slice(),
        };
        let normal_dom = DominatorTree::compute_normal(cfg);

        loop {
            let changed = backward_pass(
                ssa,
                cfg,
                &facts,
                normal_dom.reverse_postorder(),
                &mut result.live_in,
                &mut result.live_out,
            );
            if !changed {
                return result;
            }
        }
    }

    /// Return the values live at `block` entry.
    #[must_use]
    pub fn live_in(&self, block: BlockId) -> &BTreeSet<ValueId> {
        &self.live_in[block.0 as usize]
    }

    /// Return the values live at `block` exit.
    #[must_use]
    pub fn live_out(&self, block: BlockId) -> &BTreeSet<ValueId> {
        &self.live_out[block.0 as usize]
    }

    /// Return the values live immediately after one instruction.
    ///
    /// The block fixpoint is walked backwards from its live-out boundary. A
    /// result is killed at its definition and inputs become live before their
    /// use, matching the equations used by register allocation. `None` means
    /// the block or instruction index is outside the verified SSA layout.
    #[must_use]
    pub fn live_after_instruction(
        &self,
        ssa: &SsaFunction,
        block: BlockId,
        instruction_index: usize,
    ) -> Option<BTreeSet<ValueId>> {
        let block_index = block.0 as usize;
        let instructions = &ssa.blocks.get(block_index)?.instrs;
        if instruction_index >= instructions.len() {
            return None;
        }
        let mut live = self.live_out.get(block_index)?.clone();
        for instruction in instructions[instruction_index + 1..].iter().rev() {
            if let Some(result) = instruction.result {
                live.remove(&result);
            }
            live.extend(instruction.inputs.iter().copied());
        }
        Some(live)
    }

    /// Verify range, fixpoint, dominance, boundary, and use invariants.
    pub fn verify(
        &self,
        ssa: &SsaFunction,
        cfg: &ControlFlowGraph,
        full_dom: &DominatorTree,
    ) -> Result<(), LivenessError> {
        let block_count = cfg.blocks.len();
        if self.live_in.len() != block_count || self.live_out.len() != block_count {
            return Err(LivenessError::BlockCountMismatch {
                expected: block_count,
                live_in: self.live_in.len(),
                live_out: self.live_out.len(),
            });
        }
        if !full_dom.includes_exception_edges() {
            return Err(LivenessError::NormalDominatorUsedForVerification);
        }
        full_dom
            .verify(cfg)
            .map_err(LivenessError::InvalidFullDominator)?;

        let value_count = ssa.values.len();
        for block_index in 0..block_count {
            let block = BlockId(block_index as u32);
            for (set, values) in [
                (LivenessSet::LiveIn, &self.live_in[block_index]),
                (LivenessSet::LiveOut, &self.live_out[block_index]),
            ] {
                if let Some(&value) = values.iter().find(|value| value.0 as usize >= value_count) {
                    return Err(LivenessError::ValueOutOfRange {
                        block,
                        set,
                        value,
                        value_count,
                    });
                }
            }
        }

        let facts = block_facts(ssa);

        // This is deliberately separate from `backward_pass`: it directly
        // reconstructs only the successor equation from the stored live-ins.
        for block_index in 0..block_count {
            let block = BlockId(block_index as u32);
            let expected = recompute_live_out(ssa, cfg, &facts, &self.live_in, block);
            if expected != self.live_out[block_index] {
                return Err(LivenessError::LiveOutInconsistent {
                    block,
                    expected,
                    actual: self.live_out[block_index].clone(),
                });
            }
        }

        let normal_dom = DominatorTree::compute_normal(cfg);
        let mut next_live_in = self.live_in.clone();
        let mut next_live_out = self.live_out.clone();
        backward_pass(
            ssa,
            cfg,
            &facts,
            normal_dom.reverse_postorder(),
            &mut next_live_in,
            &mut next_live_out,
        );
        for block_index in 0..block_count {
            if next_live_in[block_index] != self.live_in[block_index]
                || next_live_out[block_index] != self.live_out[block_index]
            {
                return Err(LivenessError::FixpointUnstable {
                    block: BlockId(block_index as u32),
                    expected_live_in: next_live_in[block_index].clone(),
                    actual_live_in: self.live_in[block_index].clone(),
                    expected_live_out: next_live_out[block_index].clone(),
                    actual_live_out: self.live_out[block_index].clone(),
                });
            }
        }

        for block_index in 0..block_count {
            let block = BlockId(block_index as u32);
            for (set, values) in [
                (LivenessSet::LiveIn, &self.live_in[block_index]),
                (LivenessSet::LiveOut, &self.live_out[block_index]),
            ] {
                for &value in values {
                    let definition = ssa.values[value.0 as usize].def_block;
                    if !full_dom.dominates(definition, block) {
                        return Err(LivenessError::DefinitionDoesNotDominate {
                            block,
                            set,
                            value,
                            definition,
                        });
                    }
                }
            }
        }

        if !self.live_in[ssa.entry.0 as usize].is_empty() {
            return Err(LivenessError::EntryLiveInNotEmpty {
                values: self.live_in[ssa.entry.0 as usize].clone(),
            });
        }

        for (block_index, facts) in facts.iter().enumerate() {
            let block = BlockId(block_index as u32);
            if let Some(&value) = facts.uses.difference(&self.live_in[block_index]).next() {
                return Err(LivenessError::UpwardExposedUseMissing { block, value });
            }
        }

        Ok(())
    }
}

fn block_facts(ssa: &SsaFunction) -> Vec<BlockFacts> {
    ssa.blocks
        .iter()
        .map(|block| {
            let phi_defs: BTreeSet<_> = block.phis.iter().copied().collect();
            let mut defined = phi_defs.clone();
            let mut instr_defs = BTreeSet::new();
            let mut uses = BTreeSet::new();
            for instruction in &block.instrs {
                for &input in &instruction.inputs {
                    if !defined.contains(&input) {
                        uses.insert(input);
                    }
                }
                if let Some(result) = instruction.result {
                    defined.insert(result);
                    instr_defs.insert(result);
                }
            }
            BlockFacts {
                phi_defs,
                instr_defs,
                uses,
            }
        })
        .collect()
}

fn backward_pass(
    ssa: &SsaFunction,
    cfg: &ControlFlowGraph,
    facts: &[BlockFacts],
    reverse_postorder: &[BlockId],
    live_in: &mut [BTreeSet<ValueId>],
    live_out: &mut [BTreeSet<ValueId>],
) -> bool {
    let mut changed = false;
    for &block in reverse_postorder.iter().rev() {
        let block_index = block.0 as usize;
        let new_live_out = recompute_live_out(ssa, cfg, facts, live_in, block);
        let mut new_live_in = facts[block_index].phi_defs.clone();
        new_live_in.extend(facts[block_index].uses.iter().copied());
        new_live_in.extend(
            new_live_out
                .difference(&facts[block_index].instr_defs)
                .copied(),
        );
        if block == ssa.entry {
            new_live_in.clear();
        }

        if live_out[block_index] != new_live_out {
            live_out[block_index] = new_live_out;
            changed = true;
        }
        if live_in[block_index] != new_live_in {
            live_in[block_index] = new_live_in;
            changed = true;
        }
    }
    changed
}

fn recompute_live_out(
    ssa: &SsaFunction,
    cfg: &ControlFlowGraph,
    facts: &[BlockFacts],
    live_in: &[BTreeSet<ValueId>],
    block: BlockId,
) -> BTreeSet<ValueId> {
    let mut result = BTreeSet::new();
    for &successor in &cfg.blocks[block.0 as usize].normal_succs {
        let successor_index = successor.0 as usize;
        result.extend(
            live_in[successor_index]
                .difference(&facts[successor_index].phi_defs)
                .copied(),
        );
        result.extend(phi_uses(ssa, cfg, block, successor));
    }
    result
}

fn phi_uses(
    ssa: &SsaFunction,
    cfg: &ControlFlowGraph,
    predecessor: BlockId,
    successor: BlockId,
) -> BTreeSet<ValueId> {
    let predecessor_index = normal_predecessors(cfg, successor)
        .position(|candidate| candidate == predecessor)
        .expect("normal successor has the source block as a normal predecessor");
    ssa.blocks[successor.0 as usize]
        .phis
        .iter()
        .filter_map(|&phi| match &ssa.values[phi.0 as usize].def {
            // A spliced call's result merges the callee's returned values, so
            // like a phi it uses one input per predecessor edge.
            ValueDef::Phi { inputs, .. } | ValueDef::InlineResult { inputs, .. } => {
                Some(inputs[predecessor_index])
            }
            ValueDef::Param { .. }
            | ValueDef::Uninitialized { .. }
            | ValueDef::InlineUndefinedReturn { .. }
            | ValueDef::ExceptionInput { .. }
            | ValueDef::Op { .. } => None,
        })
        .collect()
}

fn normal_predecessors(
    cfg: &ControlFlowGraph,
    block: BlockId,
) -> impl Iterator<Item = BlockId> + '_ {
    cfg.blocks[block.0 as usize]
        .preds
        .iter()
        .copied()
        .filter(move |predecessor| {
            cfg.blocks[predecessor.0 as usize]
                .normal_succs
                .contains(&block)
        })
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{NO_HANDLER_OFFSET, Op, Operand};
    use otter_vm::{JitCompileSnapshot, jit::JitTestInstruction};

    use super::*;

    fn snapshot(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> JitCompileSnapshot {
        let instructions = instructions
            .into_iter()
            .enumerate()
            .map(|(pc, (op, operands))| {
                JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands)
            })
            .collect();
        JitCompileSnapshot::without_feedback(0, param_count, register_count, instructions)
    }

    fn analyses(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> (ControlFlowGraph, SsaFunction, DominatorTree, Liveness) {
        let snapshot = snapshot(param_count, register_count, instructions);
        let cfg = ControlFlowGraph::build(&snapshot).expect("CFG builds");
        let ssa = SsaFunction::build(&snapshot, &cfg).expect("SSA builds");
        let full_dom = DominatorTree::compute(&cfg);
        let liveness = Liveness::compute(&ssa, &cfg);
        liveness
            .verify(&ssa, &cfg, &full_dom)
            .expect("liveness verifies");
        (cfg, ssa, full_dom, liveness)
    }

    fn block_at(cfg: &ControlFlowGraph, pc: u32) -> BlockId {
        cfg.blocks
            .iter()
            .find(|block| block.start_pc == pc)
            .expect("PC starts a block")
            .id
    }

    fn op_value_at(ssa: &SsaFunction, pc: u32) -> ValueId {
        ssa.values
            .iter()
            .find_map(|value| match value.def {
                ValueDef::Op { pc: owner, .. } if owner == pc => Some(value.id),
                _ => None,
            })
            .expect("instruction has an SSA result")
    }

    fn phi_for(ssa: &SsaFunction, block: BlockId, register: u16) -> ValueId {
        ssa.blocks[block.0 as usize]
            .phis
            .iter()
            .copied()
            .find(|&value| {
                matches!(
                    ssa.values[value.0 as usize].def,
                    ValueDef::Phi {
                        register: owner,
                        ..
                    } if owner == register
                )
            })
            .expect("block has the requested phi")
    }

    #[test]
    fn straight_line_tracks_gap_last_use_and_dead_definition() {
        let (cfg, ssa, _dom, liveness) = analyses(
            1,
            4,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(7)]),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (Op::Nop, vec![]),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(9)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let value = op_value_at(&ssa, 0);
        let unused = op_value_at(&ssa, 6);
        let definition = block_at(&cfg, 0);
        let gap = block_at(&cfg, 2);
        let use_block = block_at(&cfg, 4);
        let after = block_at(&cfg, 6);

        assert!(liveness.live_out(definition).contains(&value));
        assert!(liveness.live_in(gap).contains(&value));
        assert!(liveness.live_out(gap).contains(&value));
        assert!(liveness.live_in(use_block).contains(&value));
        assert!(!liveness.live_out(use_block).contains(&value));
        assert!(!liveness.live_in(after).contains(&value));
        assert!(liveness.live_in.iter().all(|set| !set.contains(&unused)));
        assert!(liveness.live_out.iter().all(|set| !set.contains(&unused)));
    }

    #[test]
    fn instruction_boundary_live_out_keeps_only_later_uses() {
        let (cfg, ssa, _dom, liveness) = analyses(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(7)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let block = block_at(&cfg, 0);
        let loaded = op_value_at(&ssa, 0);
        let sum = op_value_at(&ssa, 1);
        let parameter = ssa.blocks[block.0 as usize].instrs[1].inputs[1];

        assert_eq!(
            liveness.live_after_instruction(&ssa, block, 0),
            Some(BTreeSet::from([loaded, parameter]))
        );
        assert_eq!(
            liveness.live_after_instruction(&ssa, block, 1),
            Some(BTreeSet::from([sum]))
        );
        assert_eq!(
            liveness.live_after_instruction(&ssa, block, 2),
            Some(BTreeSet::new())
        );
    }

    #[test]
    fn diamond_keeps_shared_value_in_both_arms_but_not_arm_local_at_join() {
        let (cfg, ssa, _dom, liveness) = analyses(
            1,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(4), Operand::Register(0)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Register(0),
                    ],
                ),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (Op::Jump, vec![Operand::Imm32(2)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (Op::ReturnUndefined, vec![]),
            ],
        );
        let shared = op_value_at(&ssa, 0);
        let arm_local = op_value_at(&ssa, 2);
        let branch = block_at(&cfg, 0);
        let first_arm = block_at(&cfg, 2);
        let second_arm = block_at(&cfg, 6);
        let join = block_at(&cfg, 8);

        assert!(liveness.live_out(branch).contains(&shared));
        assert!(liveness.live_in(first_arm).contains(&shared));
        assert!(liveness.live_in(second_arm).contains(&shared));
        assert!(!liveness.live_in(join).contains(&arm_local));
        assert!(!liveness.live_out(first_arm).contains(&arm_local));
    }

    #[test]
    fn phi_inputs_are_edge_uses_and_phi_result_is_live_in_at_merge() {
        let (cfg, ssa, _dom, liveness) = analyses(
            1,
            2,
            vec![
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(0)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(10)],
                ),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(20)],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let left = block_at(&cfg, 1);
        let right = block_at(&cfg, 3);
        let merge = block_at(&cfg, 4);
        let left_value = op_value_at(&ssa, 1);
        let right_value = op_value_at(&ssa, 3);
        let phi = phi_for(&ssa, merge, 1);

        assert!(liveness.live_out(left).contains(&left_value));
        assert!(liveness.live_out(right).contains(&right_value));
        assert!(liveness.live_in(merge).contains(&phi));
        assert!(!liveness.live_in(merge).contains(&left_value));
        assert!(!liveness.live_in(merge).contains(&right_value));
    }

    #[test]
    fn while_loop_carries_value_around_backedge() {
        let (cfg, ssa, _dom, liveness) = analyses(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(1)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (Op::Nop, vec![]),
                (Op::Jump, vec![Operand::Imm32(-4)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let header = block_at(&cfg, 2);
        let latch = block_at(&cfg, 3);
        let carried = op_value_at(&ssa, 3);
        let phi = phi_for(&ssa, header, 1);

        assert!(liveness.live_out(latch).contains(&carried));
        assert!(liveness.live_in(header).contains(&phi));
    }

    #[test]
    fn try_catch_does_not_propagate_pre_try_value_on_exception_edge() {
        let (cfg, ssa, _dom, liveness) = analyses(
            0,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
                (
                    Op::EnterTry,
                    vec![
                        Operand::Imm32(3),
                        Operand::Imm32(NO_HANDLER_OFFSET),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::LoadGlobalOrThrow,
                    vec![Operand::Register(1), Operand::ConstIndex(0)],
                ),
                (Op::LeaveTry, vec![]),
                (Op::ReturnUndefined, vec![]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::Register(0),
                    ],
                ),
                (Op::ReturnUndefined, vec![]),
            ],
        );
        let pre_try = op_value_at(&ssa, 0);
        let try_body = block_at(&cfg, 2);
        let handler = block_at(&cfg, 5);
        let handler_input = ssa.blocks[handler.0 as usize]
            .phis
            .iter()
            .copied()
            .find(|&value| {
                matches!(
                    ssa.values[value.0 as usize].def,
                    ValueDef::ExceptionInput { register: 0, .. }
                )
            })
            .expect("handler reloads register zero");

        assert!(matches!(
            ssa.values[handler_input.0 as usize].def,
            ValueDef::ExceptionInput { block, register: 0 } if block == handler
        ));
        assert!(!liveness.live_out(try_body).contains(&pre_try));
        assert!(liveness.live_in(handler).contains(&handler_input));
        assert!(!liveness.live_in(handler).contains(&pre_try));
    }

    #[test]
    fn verifier_rejects_corrupt_live_out_with_consistency_error() {
        let (cfg, ssa, full_dom, mut liveness) = analyses(
            1,
            2,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)]),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let definition = block_at(&cfg, 0);
        let value = op_value_at(&ssa, 0);
        assert!(liveness.live_out[definition.0 as usize].remove(&value));

        let mut expected = BTreeSet::new();
        expected.insert(value);
        assert_eq!(
            liveness.verify(&ssa, &cfg, &full_dom),
            Err(LivenessError::LiveOutInconsistent {
                block: definition,
                expected,
                actual: BTreeSet::new(),
            })
        );
    }
}
