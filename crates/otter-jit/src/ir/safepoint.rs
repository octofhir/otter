//! Precise, machine-independent GC safepoint roots over SSA values.
//!
//! # Contents
//! - [`SafepointRoots`] — deterministic per-instruction live-across root sets.
//! - [`SafepointPoint`] — one bytecode safepoint and its SSA roots.
//! - [`SafepointRoots::compute`] — intra-block backward live-after analysis.
//! - [`SafepointRoots::verify`] — independent completeness, liveness, range,
//!   dominance, and ordering checks.
//! - [`SafepointError`] — precise verification failures.
//!
//! # Invariants
//! - Safepoints come only from
//!   `opcode_schema(op).effects.safepoint_required`; no opcode list is copied
//!   into this module.
//! - Roots are values live immediately after the safepoint instruction, before
//!   applying that instruction's uses and definition to the backward walk.
//! - Points use full-edge block reverse-postorder and intra-block instruction
//!   order; roots use [`BTreeSet`], so all observable ordering is deterministic.
//! - Analysis and verification consume immutable IR and have no runtime or
//!   code-generation effect.
//!
//! # See also
//! - [`crate::ir::ssa`]
//! - [`crate::ir::liveness`]
//! - [`crate::ir::dom`]
//! - [`otter_bytecode::opcode_schema`]

use std::collections::{BTreeMap, BTreeSet};

use otter_bytecode::{Op, opcode_schema::opcode_schema};

use super::{
    cfg::{BlockId, ControlFlowGraph},
    dom::{DomError, DominatorTree},
    liveness::Liveness,
    ssa::{SsaFunction, ValueId},
};

/// Precise SSA roots for every safepoint instruction in one function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafepointRoots {
    points: Box<[SafepointPoint]>,
}

/// One safepoint instruction and the values live immediately after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafepointPoint {
    /// CFG block containing the safepoint.
    pub block: BlockId,
    /// Canonical bytecode PC of the safepoint instruction.
    pub pc: u32,
    /// Original bytecode opcode.
    pub op: Op,
    /// SSA values live across this safepoint, in deterministic value-id order.
    pub roots: BTreeSet<ValueId>,
}

/// Failure to verify stored safepoint roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafepointError {
    /// Verification was given a normal-edge-only dominator tree.
    NormalDominatorUsedForVerification,
    /// The supplied full-edge dominator tree is internally invalid.
    InvalidFullDominator(DomError),
    /// Stored points do not contain the expected number of safepoints.
    PointCountMismatch {
        /// Number of schema-required safepoint instructions.
        expected: usize,
        /// Number of stored points.
        actual: usize,
    },
    /// The same instruction occurs more than once in the stored point set.
    DuplicatePoint {
        /// Block containing the duplicate.
        block: BlockId,
        /// Canonical instruction PC of the duplicate.
        pc: u32,
    },
    /// A stored point's opcode does not require a safepoint.
    PointDoesNotRequireSafepoint {
        /// Point position in stored order.
        index: usize,
        /// Block containing the point.
        block: BlockId,
        /// Canonical instruction PC.
        pc: u32,
        /// Non-safepoint opcode.
        op: Op,
    },
    /// A stored location is not one of the SSA function's safepoints.
    UnexpectedPoint {
        /// Point position in stored order.
        index: usize,
        /// Stored block.
        block: BlockId,
        /// Stored canonical instruction PC.
        pc: u32,
    },
    /// A stored point's opcode differs from the SSA instruction at its location.
    PointOpcodeMismatch {
        /// Point position in stored order.
        index: usize,
        /// Point block.
        block: BlockId,
        /// Canonical instruction PC.
        pc: u32,
        /// Opcode from SSA.
        expected: Op,
        /// Stored opcode.
        actual: Op,
    },
    /// Stored points are not in full-edge block RPO and instruction order.
    PointOrderNotDeterministic {
        /// First position whose point differs from canonical order.
        index: usize,
        /// Canonical block at this position.
        expected_block: BlockId,
        /// Canonical PC at this position.
        expected_pc: u32,
        /// Stored block at this position.
        actual_block: BlockId,
        /// Stored PC at this position.
        actual_pc: u32,
    },
    /// A root is outside dense SSA value storage.
    RootValueOutOfRange {
        /// Point containing the invalid root.
        block: BlockId,
        /// Canonical safepoint PC.
        pc: u32,
        /// Invalid value identity.
        value: ValueId,
        /// Number of valid SSA values.
        value_count: usize,
    },
    /// Root iteration is not strictly increasing by value identity.
    RootsNotDeterministic {
        /// Point containing the non-canonical set.
        block: BlockId,
        /// Canonical safepoint PC.
        pc: u32,
        /// Earlier value encountered during iteration.
        previous: ValueId,
        /// Later value that does not strictly follow it.
        current: ValueId,
    },
    /// A root's defining block does not dominate its safepoint block.
    RootDefinitionDoesNotDominate {
        /// Point containing the invalid root.
        block: BlockId,
        /// Canonical safepoint PC.
        pc: u32,
        /// Invalid root value.
        value: ValueId,
        /// Block defining the root value.
        definition: BlockId,
    },
    /// Stored roots differ from an independent live-after recomputation.
    RootsInconsistent {
        /// Point containing inconsistent roots.
        block: BlockId,
        /// Canonical safepoint PC.
        pc: u32,
        /// Independently recomputed roots.
        expected: BTreeSet<ValueId>,
        /// Stored roots.
        actual: BTreeSet<ValueId>,
    },
}

impl std::fmt::Display for SafepointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid SSA safepoint roots: {self:?}")
    }
}

impl std::error::Error for SafepointError {}

impl SafepointRoots {
    /// Compute roots live immediately after every schema-required safepoint.
    #[must_use]
    pub fn compute(ssa: &SsaFunction, cfg: &ControlFlowGraph, liveness: &Liveness) -> Self {
        let full_dom = DominatorTree::compute(cfg);
        let mut points = Vec::new();

        for &block in full_dom.reverse_postorder() {
            let mut live_after = liveness.live_out(block).clone();
            let mut block_points = Vec::new();
            for instruction in ssa.blocks[block.0 as usize].instrs.iter().rev() {
                if opcode_schema(instruction.op).effects.safepoint_required {
                    block_points.push(SafepointPoint {
                        block,
                        pc: instruction.pc,
                        op: instruction.op,
                        roots: live_after.clone(),
                    });
                }

                if let Some(result) = instruction.result {
                    live_after.remove(&result);
                }
                live_after.extend(instruction.inputs.iter().copied());
            }
            points.extend(block_points.into_iter().rev());
        }

        Self {
            points: points.into_boxed_slice(),
        }
    }

    /// Return all safepoints in deterministic block-RPO/instruction order.
    #[must_use]
    pub fn points(&self) -> &[SafepointPoint] {
        &self.points
    }

    /// Return the safepoint at canonical bytecode `pc`, when present.
    #[must_use]
    pub fn at(&self, pc: u32) -> Option<&SafepointPoint> {
        self.points.iter().find(|point| point.pc == pc)
    }

    /// Verify point completeness, ordering, roots, range, and dominance.
    pub fn verify(
        &self,
        ssa: &SsaFunction,
        cfg: &ControlFlowGraph,
        liveness: &Liveness,
        full_dom: &DominatorTree,
    ) -> Result<(), SafepointError> {
        if !full_dom.includes_exception_edges() {
            return Err(SafepointError::NormalDominatorUsedForVerification);
        }
        full_dom
            .verify(cfg)
            .map_err(SafepointError::InvalidFullDominator)?;

        let mut expected_points = Vec::new();
        let mut expected_by_location = BTreeMap::new();
        for &block in full_dom.reverse_postorder() {
            for instruction in &ssa.blocks[block.0 as usize].instrs {
                if opcode_schema(instruction.op).effects.safepoint_required {
                    expected_points.push((block, instruction.pc, instruction.op));
                    expected_by_location.insert((block, instruction.pc), instruction.op);
                }
            }
        }

        if self.points.len() != expected_points.len() {
            return Err(SafepointError::PointCountMismatch {
                expected: expected_points.len(),
                actual: self.points.len(),
            });
        }

        let mut actual_locations = BTreeSet::new();
        for (index, point) in self.points.iter().enumerate() {
            if !actual_locations.insert((point.block, point.pc)) {
                return Err(SafepointError::DuplicatePoint {
                    block: point.block,
                    pc: point.pc,
                });
            }
            if !opcode_schema(point.op).effects.safepoint_required {
                return Err(SafepointError::PointDoesNotRequireSafepoint {
                    index,
                    block: point.block,
                    pc: point.pc,
                    op: point.op,
                });
            }
            let Some(&expected_op) = expected_by_location.get(&(point.block, point.pc)) else {
                return Err(SafepointError::UnexpectedPoint {
                    index,
                    block: point.block,
                    pc: point.pc,
                });
            };
            if point.op != expected_op {
                return Err(SafepointError::PointOpcodeMismatch {
                    index,
                    block: point.block,
                    pc: point.pc,
                    expected: expected_op,
                    actual: point.op,
                });
            }
        }

        for (index, (point, &(expected_block, expected_pc, _))) in
            self.points.iter().zip(&expected_points).enumerate()
        {
            if point.block != expected_block || point.pc != expected_pc {
                return Err(SafepointError::PointOrderNotDeterministic {
                    index,
                    expected_block,
                    expected_pc,
                    actual_block: point.block,
                    actual_pc: point.pc,
                });
            }
        }

        let value_count = ssa.values.len();
        for point in &self.points {
            let mut previous = None;
            for &value in &point.roots {
                if value.0 as usize >= value_count {
                    return Err(SafepointError::RootValueOutOfRange {
                        block: point.block,
                        pc: point.pc,
                        value,
                        value_count,
                    });
                }
                if let Some(previous) = previous
                    && previous >= value
                {
                    return Err(SafepointError::RootsNotDeterministic {
                        block: point.block,
                        pc: point.pc,
                        previous,
                        current: value,
                    });
                }
                previous = Some(value);

                let definition = ssa.values[value.0 as usize].def_block;
                if !full_dom.dominates(definition, point.block) {
                    return Err(SafepointError::RootDefinitionDoesNotDominate {
                        block: point.block,
                        pc: point.pc,
                        value,
                        definition,
                    });
                }
            }
        }

        // Deliberately independent of `compute`: reconstruct every live-after
        // set in a separate backward walk before consulting stored roots.
        let mut recomputed = BTreeMap::new();
        for &block in full_dom.reverse_postorder() {
            let mut live = liveness.live_out(block).clone();
            for instruction in ssa.blocks[block.0 as usize].instrs.iter().rev() {
                if opcode_schema(instruction.op).effects.safepoint_required {
                    recomputed.insert((block, instruction.pc), live.clone());
                }
                if let Some(definition) = instruction.result {
                    live.remove(&definition);
                }
                for &used in &instruction.inputs {
                    live.insert(used);
                }
            }
        }

        for point in &self.points {
            let expected = recomputed
                .get(&(point.block, point.pc))
                .expect("point-set verification established this safepoint location");
            if point.roots != *expected {
                return Err(SafepointError::RootsInconsistent {
                    block: point.block,
                    pc: point.pc,
                    expected: expected.clone(),
                    actual: point.roots.clone(),
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{Op, Operand, opcode_schema::opcode_schema};
    use otter_vm::{JitCompileSnapshot, jit::JitTestInstruction};

    use super::*;
    use crate::ir::ssa::ValueDef;

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
    ) -> (
        ControlFlowGraph,
        SsaFunction,
        DominatorTree,
        Liveness,
        SafepointRoots,
    ) {
        let snapshot = snapshot(param_count, register_count, instructions);
        let cfg = ControlFlowGraph::build(&snapshot).expect("CFG builds");
        let ssa = SsaFunction::build(&snapshot, &cfg).expect("SSA builds");
        let full_dom = DominatorTree::compute(&cfg);
        ssa.verify(&cfg, &full_dom).expect("SSA verifies");
        let liveness = Liveness::compute(&ssa, &cfg);
        liveness
            .verify(&ssa, &cfg, &full_dom)
            .expect("liveness verifies");
        let roots = SafepointRoots::compute(&ssa, &cfg, &liveness);
        roots
            .verify(&ssa, &cfg, &liveness, &full_dom)
            .expect("safepoint roots verify");
        (cfg, ssa, full_dom, liveness, roots)
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

    fn assert_safepoint(op: Op) {
        assert!(
            opcode_schema(op).effects.safepoint_required,
            "test opcode {op:?} must require a safepoint"
        );
    }

    #[test]
    fn value_live_across_is_root_and_dead_value_is_not() {
        assert_safepoint(Op::NewObject);
        let (_cfg, ssa, _dom, _liveness, roots) = analyses(
            0,
            3,
            vec![
                (Op::LoadUndefined, vec![Operand::Register(0)]),
                (Op::LoadUndefined, vec![Operand::Register(1)]),
                (Op::NewObject, vec![Operand::Register(2)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );
        let live = op_value_at(&ssa, 0);
        let dead = op_value_at(&ssa, 1);
        let point = roots.at(2).expect("allocation is a safepoint");

        assert!(point.roots.contains(&live));
        assert!(!point.roots.contains(&dead));
    }

    #[test]
    fn straight_line_without_required_op_has_no_safepoints() {
        assert!(!opcode_schema(Op::LoadUndefined).effects.safepoint_required);
        assert!(!opcode_schema(Op::Nop).effects.safepoint_required);
        let (_cfg, _ssa, _dom, _liveness, roots) = analyses(
            0,
            1,
            vec![
                (Op::LoadUndefined, vec![Operand::Register(0)]),
                (Op::Nop, vec![]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );

        assert!(roots.points().is_empty());
    }

    #[test]
    fn back_to_back_safepoints_keep_only_values_live_across_each() {
        assert_safepoint(Op::NewArray);
        assert_safepoint(Op::NewObject);
        let (_cfg, ssa, _dom, _liveness, roots) = analyses(
            0,
            4,
            vec![
                (Op::LoadUndefined, vec![Operand::Register(0)]),
                (Op::LoadUndefined, vec![Operand::Register(1)]),
                (
                    Op::NewArray,
                    vec![
                        Operand::Register(2),
                        Operand::ConstIndex(1),
                        Operand::Register(1),
                    ],
                ),
                (Op::NewObject, vec![Operand::Register(3)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );
        let across_both = op_value_at(&ssa, 0);
        let consumed_at_first = op_value_at(&ssa, 1);
        let first = roots.at(2).expect("first allocation is a safepoint");
        let second = roots.at(3).expect("second allocation is a safepoint");

        assert!(first.roots.contains(&across_both));
        assert!(second.roots.contains(&across_both));
        assert!(!second.roots.contains(&consumed_at_first));
    }

    #[test]
    fn loop_carried_value_is_root_at_body_safepoint() {
        assert_safepoint(Op::NewObject);
        let (_cfg, ssa, _dom, _liveness, roots) = analyses(
            0,
            3,
            vec![
                (Op::LoadUndefined, vec![Operand::Register(1)]),
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
                (Op::NewObject, vec![Operand::Register(2)]),
                (Op::Jump, vec![Operand::Imm32(-4)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let carried = op_value_at(&ssa, 3);
        let body_point = roots.at(4).expect("body allocation is a safepoint");

        assert!(body_point.roots.contains(&carried));
    }

    #[test]
    fn allocation_result_used_later_is_its_own_safepoint_root() {
        assert_safepoint(Op::NewObject);
        let (_cfg, ssa, _dom, _liveness, roots) = analyses(
            0,
            1,
            vec![
                (Op::NewObject, vec![Operand::Register(0)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );
        let allocation = op_value_at(&ssa, 0);

        assert!(
            roots
                .at(0)
                .expect("allocation is a safepoint")
                .roots
                .contains(&allocation)
        );
    }

    #[test]
    fn verifier_rejects_missing_live_across_root() {
        assert_safepoint(Op::NewObject);
        let (cfg, ssa, full_dom, liveness, mut roots) = analyses(
            0,
            2,
            vec![
                (Op::LoadUndefined, vec![Operand::Register(0)]),
                (Op::NewObject, vec![Operand::Register(1)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );
        let needed = op_value_at(&ssa, 0);
        assert!(roots.points[0].roots.remove(&needed));

        assert_eq!(
            roots.verify(&ssa, &cfg, &liveness, &full_dom),
            Err(SafepointError::RootsInconsistent {
                block: roots.points[0].block,
                pc: 1,
                expected: BTreeSet::from([needed]),
                actual: BTreeSet::new(),
            })
        );
    }
}
