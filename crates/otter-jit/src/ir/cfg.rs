//! Backend-independent control-flow graph over one verified bytecode function.
//!
//! # Contents
//! - [`BlockId`] — dense block identity.
//! - [`Terminator`] and [`Block`] — typed block endings and complete edges.
//! - [`ControlFlowGraph`] — deterministic graph construction and verification.
//! - [`CfgError`] — precise construction and graph-integrity failures.
//!
//! # Invariants
//! - Block leaders, loop headers, and exception ranges come from the VM-owned
//!   [`otter_vm::CodeBlock`] control-flow table.
//! - Normal successors come only from the authoritative opcode schema.
//! - Blocks and all edge lists are sorted, dense, and duplicate-free.
//! - Exception edges target the innermost enclosing catch or finally handler.
//!
//! # See also
//! - [`otter_bytecode::opcode_schema`]
//! - [`otter_vm::CodeBlockControlFlowView`]

use std::collections::{BTreeMap, BTreeSet};

use otter_bytecode::{
    Op,
    opcode_schema::{ControlFlow, SuccessorSpec, opcode_schema},
};
use otter_vm::JitCompileSnapshot;
use smallvec::{SmallVec, smallvec};

/// Dense control-flow block identity. The function entry is always `BlockId(0)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u32);

/// Typed control-flow behavior of a block's final instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    /// The block ends because the following instruction is another leader.
    FallThrough,
    /// Unconditional `Jump` or `JumpViaFinally`.
    Jump,
    /// Conditional branch with semantic target identities preserved.
    Branch {
        /// Block selected when the condition takes the encoded branch.
        taken: BlockId,
        /// Block containing the following instruction.
        fallthrough: BlockId,
    },
    /// Ordinary function return.
    Return,
    /// Proper tail call that completes the current frame.
    TailCall,
    /// Explicit exception throw.
    Throw,
    /// Await or generator suspension with a resumable successor.
    Suspend,
}

/// One basic block and all of its normal, exceptional, and incoming edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Dense block identity, equal to this block's graph index.
    pub id: BlockId,
    /// Canonical instruction PC of the leader.
    pub start_pc: u32,
    /// Canonical instruction PCs in this block, in ascending order.
    pub instr_pcs: Vec<u32>,
    /// Typed behavior of the final instruction.
    pub terminator: Terminator,
    /// Normal-flow successors, sorted and duplicate-free.
    pub normal_succs: SmallVec<[BlockId; 2]>,
    /// Enclosing handler entries reachable on exception, sorted and duplicate-free.
    pub exception_succs: SmallVec<[BlockId; 2]>,
    /// Union of normal and exception predecessors, sorted and duplicate-free.
    pub preds: SmallVec<[BlockId; 4]>,
    /// Whether this leader is targeted by a backwards normal-flow edge.
    pub is_loop_header: bool,
}

/// Complete control-flow graph for one bytecode function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlFlowGraph {
    /// Blocks in ascending leader-PC order; index equals `BlockId.0`.
    pub blocks: Vec<Block>,
    /// Function entry, always `BlockId(0)`.
    pub entry: BlockId,
}

/// Failure to construct or verify a bytecode control-flow graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfgError {
    /// The source function contains no instructions.
    EmptyFunction,
    /// Entry identity is not `BlockId(0)`.
    EntryNotZero {
        /// Invalid entry identity.
        entry: BlockId,
    },
    /// The entry block does not start at the function's first canonical PC.
    EntryStartMismatch {
        /// Expected first PC.
        expected: u32,
        /// Actual entry start PC.
        actual: u32,
    },
    /// A block id differs from its dense vector index.
    BlockIdMismatch {
        /// Vector index represented as a block id.
        expected: BlockId,
        /// Identity stored in the block.
        actual: BlockId,
    },
    /// Block start PCs are not strictly ascending.
    BlockOrder {
        /// Earlier block identity.
        previous: BlockId,
        /// Later block identity that violates ordering.
        current: BlockId,
    },
    /// Block starts differ from the authoritative leader set.
    LeaderSetMismatch,
    /// One block contains no instructions.
    EmptyBlock {
        /// Empty block identity.
        block: BlockId,
    },
    /// An instruction PC occurs more than once or moves backwards.
    InstructionOverlap {
        /// Offending canonical PC.
        pc: u32,
    },
    /// Canonical instruction coverage contains a gap.
    InstructionGap {
        /// First missing canonical PC.
        expected: u32,
        /// Next PC found in the graph.
        actual: u32,
    },
    /// Snapshot metadata does not identify the expected canonical instruction.
    InstructionMetadataMismatch {
        /// Dense metadata index.
        index: u32,
        /// Canonical PC reported by that metadata entry.
        pc: u32,
    },
    /// A schema-declared successor operand is absent or has the wrong kind.
    InvalidSuccessorOperand {
        /// Instruction owning the operand.
        pc: u32,
        /// Operand position.
        operand_index: usize,
    },
    /// A schema-declared successor points outside the function.
    InvalidSuccessorPc {
        /// Instruction owning the successor.
        pc: u32,
        /// Resolved target coordinate.
        target: i64,
    },
    /// A resolved target does not name an authoritative block leader.
    MissingTargetBlock {
        /// Resolved canonical target PC.
        target_pc: u32,
    },
    /// A successor or predecessor block id lies outside the graph.
    EdgeOutOfRange {
        /// Block containing the invalid edge.
        block: BlockId,
        /// Invalid edge endpoint.
        edge: BlockId,
    },
    /// An edge list is not sorted and duplicate-free.
    EdgeListNotCanonical {
        /// Block containing the edge list.
        block: BlockId,
    },
    /// Stored predecessors are not exactly the reverse of all successors.
    PredecessorMismatch {
        /// Block whose predecessor list is incorrect.
        block: BlockId,
    },
    /// A terminator is inconsistent with its required normal-flow shape.
    TerminatorMismatch {
        /// Block with the inconsistent terminator.
        block: BlockId,
    },
    /// A conditional branch's fields and normal successors disagree.
    BranchSuccessorMismatch {
        /// Conditional branch block.
        block: BlockId,
    },
    /// A block cannot be reached from entry over normal or exception edges.
    UnreachableBlock {
        /// Unreachable block identity.
        block: BlockId,
    },
    /// A loop-header flag is missing for a backwards normal-flow target.
    LoopHeaderFlagMismatch {
        /// Backwards-edge target block.
        block: BlockId,
    },
    /// A flagged loop header has no predecessor through a backwards edge.
    LoopHeaderWithoutBackEdge {
        /// Invalid loop header.
        block: BlockId,
    },
}

impl std::fmt::Display for CfgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid control-flow graph: {self:?}")
    }
}

impl std::error::Error for CfgError {}

impl ControlFlowGraph {
    /// Build a deterministic graph from an immutable VM compile snapshot.
    pub fn build(view: &JitCompileSnapshot) -> Result<Self, CfgError> {
        let code_block = view.code_block.as_ref();
        let instruction_count =
            u32::try_from(view.instructions.len()).map_err(|_| CfgError::InvalidSuccessorPc {
                pc: u32::MAX,
                target: i64::MAX,
            })?;
        if instruction_count == 0 {
            return Err(CfgError::EmptyFunction);
        }
        for (index, instruction) in view.instructions.iter().enumerate() {
            let index = index as u32;
            let pc = instruction.instruction_pc(code_block);
            if pc != index {
                return Err(CfgError::InstructionMetadataMismatch { index, pc });
            }
        }

        let control_flow = code_block.control_flow();
        let leaders = control_flow.block_starts();
        if leaders.first() != Some(&0) {
            return Err(CfgError::LeaderSetMismatch);
        }
        let mut block_by_pc = BTreeMap::new();
        for (index, &pc) in leaders.iter().enumerate() {
            block_by_pc.insert(pc, BlockId(index as u32));
        }

        let mut blocks = Vec::with_capacity(leaders.len());
        for (index, &start_pc) in leaders.iter().enumerate() {
            let end_pc = leaders.get(index + 1).copied().unwrap_or(instruction_count);
            if start_pc >= end_pc || end_pc > instruction_count {
                return Err(CfgError::LeaderSetMismatch);
            }
            let id = BlockId(index as u32);
            let last_pc = end_pc - 1;
            let last_instruction = &view.instructions[last_pc as usize];
            let last_op = last_instruction.op(code_block);
            let schema = opcode_schema(last_op);
            let mut normal_succs = SmallVec::new();
            let mut taken = None;
            let mut fallthrough = None;
            for successor in schema.successor_shape.exact() {
                let target_pc = match successor {
                    SuccessorSpec::Fallthrough => {
                        let target = i64::from(last_pc) + 1;
                        if target >= i64::from(instruction_count) {
                            return Err(CfgError::InvalidSuccessorPc {
                                pc: last_pc,
                                target,
                            });
                        }
                        fallthrough = Some(target as u32);
                        target as u32
                    }
                    SuccessorSpec::RelativeTarget { operand_index, .. } => {
                        let delta = last_instruction.imm32(code_block, *operand_index).ok_or(
                            CfgError::InvalidSuccessorOperand {
                                pc: last_pc,
                                operand_index: *operand_index,
                            },
                        )?;
                        let target = i64::from(last_pc) + 1 + i64::from(delta);
                        if !(0..i64::from(instruction_count)).contains(&target) {
                            return Err(CfgError::InvalidSuccessorPc {
                                pc: last_pc,
                                target,
                            });
                        }
                        taken = Some(target as u32);
                        target as u32
                    }
                    SuccessorSpec::FrameReturn => continue,
                };
                normal_succs.push(
                    *block_by_pc
                        .get(&target_pc)
                        .ok_or(CfgError::MissingTargetBlock { target_pc })?,
                );
            }
            canonicalize(&mut normal_succs);

            let terminator = classify_terminator(
                id,
                last_op,
                schema.control_flow,
                taken,
                fallthrough,
                &block_by_pc,
            )?;
            let mut exception_succs = SmallVec::new();
            let may_unwind = (start_pc..end_pc).any(|pc| {
                !opcode_schema(view.instructions[pc as usize].op(code_block))
                    .exception_successor_shape
                    .exact()
                    .is_empty()
            });
            if may_unwind
                && let Some(region) = control_flow.enclosing_exception_region(start_pc)
                && let Some(handler_pc) = region.catch_pc.or(region.finally_pc)
            {
                exception_succs.push(*block_by_pc.get(&handler_pc).ok_or(
                    CfgError::MissingTargetBlock {
                        target_pc: handler_pc,
                    },
                )?);
            }
            canonicalize(&mut exception_succs);

            blocks.push(Block {
                id,
                start_pc,
                instr_pcs: (start_pc..end_pc).collect(),
                terminator,
                normal_succs,
                exception_succs,
                preds: SmallVec::new(),
                is_loop_header: control_flow.loop_headers().binary_search(&start_pc).is_ok(),
            });
        }

        let edge_sets: Vec<_> = blocks
            .iter()
            .map(|block| {
                block
                    .normal_succs
                    .iter()
                    .chain(&block.exception_succs)
                    .copied()
                    .collect::<BTreeSet<_>>()
            })
            .collect();
        for (pred_index, successors) in edge_sets.into_iter().enumerate() {
            for successor in successors {
                blocks[successor.0 as usize]
                    .preds
                    .push(BlockId(pred_index as u32));
            }
        }
        for block in &mut blocks {
            canonicalize(&mut block.preds);
        }

        let graph = Self {
            blocks,
            entry: BlockId(0),
        };
        graph.verify()?;
        Ok(graph)
    }

    /// Verify graph structure without consulting mutable runtime state.
    pub fn verify(&self) -> Result<(), CfgError> {
        if self.blocks.is_empty() {
            return Err(CfgError::EmptyFunction);
        }
        if self.entry != BlockId(0) {
            return Err(CfgError::EntryNotZero { entry: self.entry });
        }
        if self.blocks[0].start_pc != 0 {
            return Err(CfgError::EntryStartMismatch {
                expected: 0,
                actual: self.blocks[0].start_pc,
            });
        }

        let block_count = self.blocks.len();
        let mut expected_pc = 0;
        for (index, block) in self.blocks.iter().enumerate() {
            let expected_id = BlockId(index as u32);
            if block.id != expected_id {
                return Err(CfgError::BlockIdMismatch {
                    expected: expected_id,
                    actual: block.id,
                });
            }
            if index > 0 && self.blocks[index - 1].start_pc >= block.start_pc {
                return Err(CfgError::BlockOrder {
                    previous: BlockId(index as u32 - 1),
                    current: block.id,
                });
            }
            let Some(&first_pc) = block.instr_pcs.first() else {
                return Err(CfgError::EmptyBlock { block: block.id });
            };
            if first_pc != block.start_pc {
                return Err(CfgError::LeaderSetMismatch);
            }
            for &pc in &block.instr_pcs {
                if pc < expected_pc {
                    return Err(CfgError::InstructionOverlap { pc });
                }
                if pc > expected_pc {
                    return Err(CfgError::InstructionGap {
                        expected: expected_pc,
                        actual: pc,
                    });
                }
                expected_pc = expected_pc
                    .checked_add(1)
                    .ok_or(CfgError::InstructionOverlap { pc })?;
            }
            for &edge in block
                .normal_succs
                .iter()
                .chain(&block.exception_succs)
                .chain(&block.preds)
            {
                if edge.0 as usize >= block_count {
                    return Err(CfgError::EdgeOutOfRange {
                        block: block.id,
                        edge,
                    });
                }
            }
            if !is_canonical(&block.normal_succs)
                || !is_canonical(&block.exception_succs)
                || !is_canonical(&block.preds)
            {
                return Err(CfgError::EdgeListNotCanonical { block: block.id });
            }
        }

        let mut expected_preds = vec![BTreeSet::new(); block_count];
        for block in &self.blocks {
            for &successor in block.normal_succs.iter().chain(&block.exception_succs) {
                expected_preds[successor.0 as usize].insert(block.id);
            }
        }
        for (index, block) in self.blocks.iter().enumerate() {
            if block.preds.iter().copied().collect::<BTreeSet<_>>() != expected_preds[index] {
                return Err(CfgError::PredecessorMismatch { block: block.id });
            }
            verify_terminator_shape(self, block)?;
        }

        let mut reachable = vec![false; block_count];
        let mut pending = vec![self.entry];
        while let Some(block_id) = pending.pop() {
            let index = block_id.0 as usize;
            if std::mem::replace(&mut reachable[index], true) {
                continue;
            }
            pending.extend(
                self.blocks[index]
                    .normal_succs
                    .iter()
                    .chain(&self.blocks[index].exception_succs)
                    .rev()
                    .copied(),
            );
        }
        if let Some(index) = reachable.iter().position(|reachable| !reachable) {
            return Err(CfgError::UnreachableBlock {
                block: BlockId(index as u32),
            });
        }

        for block in &self.blocks {
            let has_back_edge = self.blocks.iter().any(|pred| {
                pred.start_pc >= block.start_pc && pred.normal_succs.contains(&block.id)
            });
            if has_back_edge && !block.is_loop_header {
                return Err(CfgError::LoopHeaderFlagMismatch { block: block.id });
            }
            if block.is_loop_header && !has_back_edge {
                return Err(CfgError::LoopHeaderWithoutBackEdge { block: block.id });
            }
        }
        Ok(())
    }
}

fn classify_terminator(
    block: BlockId,
    op: Op,
    control_flow: ControlFlow,
    taken_pc: Option<u32>,
    fallthrough_pc: Option<u32>,
    block_by_pc: &BTreeMap<u32, BlockId>,
) -> Result<Terminator, CfgError> {
    Ok(match control_flow {
        ControlFlow::Fallthrough | ControlFlow::Call | ControlFlow::ExceptionRegion => {
            Terminator::FallThrough
        }
        ControlFlow::Jump => Terminator::Jump,
        ControlFlow::Branch => {
            let taken = taken_pc
                .and_then(|pc| block_by_pc.get(&pc).copied())
                .ok_or(CfgError::BranchSuccessorMismatch { block })?;
            let fallthrough = fallthrough_pc
                .and_then(|pc| block_by_pc.get(&pc).copied())
                .ok_or(CfgError::BranchSuccessorMismatch { block })?;
            Terminator::Branch { taken, fallthrough }
        }
        ControlFlow::Return if op == Op::TailCall => Terminator::TailCall,
        ControlFlow::Return => Terminator::Return,
        ControlFlow::Throw => Terminator::Throw,
        ControlFlow::Suspend => Terminator::Suspend,
    })
}

fn verify_terminator_shape(graph: &ControlFlowGraph, block: &Block) -> Result<(), CfgError> {
    let next = graph
        .blocks
        .get(block.id.0 as usize + 1)
        .map(|next| next.id);
    match block.terminator {
        Terminator::FallThrough | Terminator::TailCall | Terminator::Suspend => {
            if next.is_none() || block.normal_succs.as_slice() != next.as_slice() {
                return Err(CfgError::TerminatorMismatch { block: block.id });
            }
        }
        Terminator::Jump => {
            if block.normal_succs.len() != 1 {
                return Err(CfgError::TerminatorMismatch { block: block.id });
            }
        }
        Terminator::Branch { taken, fallthrough } => {
            if taken.0 as usize >= graph.blocks.len()
                || fallthrough.0 as usize >= graph.blocks.len()
            {
                return Err(CfgError::EdgeOutOfRange {
                    block: block.id,
                    edge: if taken.0 as usize >= graph.blocks.len() {
                        taken
                    } else {
                        fallthrough
                    },
                });
            }
            let mut expected: SmallVec<[BlockId; 2]> = smallvec![taken, fallthrough];
            canonicalize(&mut expected);
            if block.normal_succs != expected {
                return Err(CfgError::BranchSuccessorMismatch { block: block.id });
            }
        }
        Terminator::Return | Terminator::Throw => {
            if !block.normal_succs.is_empty() {
                return Err(CfgError::TerminatorMismatch { block: block.id });
            }
        }
    }
    Ok(())
}

fn canonicalize<const N: usize>(edges: &mut SmallVec<[BlockId; N]>)
where
    [BlockId; N]: smallvec::Array<Item = BlockId>,
{
    edges.sort_unstable();
    edges.dedup();
}

fn is_canonical<const N: usize>(edges: &SmallVec<[BlockId; N]>) -> bool
where
    [BlockId; N]: smallvec::Array<Item = BlockId>,
{
    edges.windows(2).all(|pair| pair[0] < pair[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{NO_HANDLER_OFFSET, Operand};
    use otter_vm::jit::JitTestInstruction;

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

    fn graph(instructions: Vec<(Op, Vec<Operand>)>) -> ControlFlowGraph {
        let snapshot = snapshot(instructions);
        let graph = ControlFlowGraph::build(&snapshot).expect("CFG builds");
        graph.verify().expect("CFG verifies");
        graph
    }

    #[test]
    fn straight_line_cfg() {
        let cfg = graph(vec![
            (Op::LoadUndefined, vec![Operand::Register(0)]),
            (Op::ReturnUndefined, vec![]),
        ]);

        assert_eq!(cfg.blocks.len(), 1);
        assert_eq!(cfg.blocks[0].instr_pcs, [0, 1]);
        assert_eq!(cfg.blocks[0].terminator, Terminator::Return);
        assert!(cfg.blocks[0].normal_succs.is_empty());
    }

    #[test]
    fn if_else_cfg() {
        let cfg = graph(vec![
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(0)],
            ),
            (Op::Nop, vec![]),
            (Op::Jump, vec![Operand::Imm32(1)]),
            (Op::Nop, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);

        assert_eq!(
            cfg.blocks[0].terminator,
            Terminator::Branch {
                taken: BlockId(2),
                fallthrough: BlockId(1),
            }
        );
        assert_eq!(cfg.blocks[3].preds.as_slice(), &[BlockId(1), BlockId(2)]);
    }

    #[test]
    fn while_loop_cfg() {
        let cfg = graph(vec![
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(0)],
            ),
            (Op::Nop, vec![]),
            (Op::Jump, vec![Operand::Imm32(-3)]),
            (Op::ReturnUndefined, vec![]),
        ]);

        assert!(cfg.blocks[0].is_loop_header);
        assert!(cfg.blocks[0].preds.contains(&BlockId(1)));
        assert!(cfg.blocks[1].start_pc >= cfg.blocks[0].start_pc);
    }

    #[test]
    fn nested_loops_cfg() {
        let cfg = graph(vec![
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

        let headers: Vec<_> = cfg
            .blocks
            .iter()
            .filter(|block| block.is_loop_header)
            .map(|block| block.start_pc)
            .collect();
        assert_eq!(headers, [0, 1]);
    }

    #[test]
    fn try_catch_cfg() {
        let cfg = graph(vec![
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
            (Op::Jump, vec![Operand::Imm32(2)]),
            (Op::Nop, vec![]),
            (Op::Nop, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);

        let body = cfg.blocks.iter().find(|block| block.start_pc == 1).unwrap();
        let catch = cfg.blocks.iter().find(|block| block.start_pc == 4).unwrap();
        assert_eq!(body.exception_succs.as_slice(), &[catch.id]);
        assert!(catch.preds.contains(&body.id));
    }

    #[test]
    fn try_finally_cfg() {
        let cfg = graph(vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Imm32(2),
                    Operand::Register(1),
                ],
            ),
            (
                Op::LoadGlobalOrThrow,
                vec![Operand::Register(0), Operand::ConstIndex(0)],
            ),
            (Op::LeaveTry, vec![]),
            (Op::Nop, vec![]),
            (Op::EndFinally, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);

        let body = cfg.blocks.iter().find(|block| block.start_pc == 1).unwrap();
        let finally = cfg.blocks.iter().find(|block| block.start_pc == 3).unwrap();
        assert_eq!(body.exception_succs.as_slice(), &[finally.id]);
        assert!(finally.preds.contains(&body.id));
    }

    #[test]
    fn early_return_inside_loop_cfg() {
        let cfg = graph(vec![
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(4), Operand::Register(0)],
            ),
            (
                Op::JumpIfTrue,
                vec![Operand::Imm32(2), Operand::Register(1)],
            ),
            (Op::Nop, vec![]),
            (Op::Jump, vec![Operand::Imm32(-4)]),
            (Op::ReturnUndefined, vec![]),
            (Op::ReturnUndefined, vec![]),
        ]);

        let early_return = cfg.blocks.iter().find(|block| block.start_pc == 4).unwrap();
        assert_eq!(early_return.terminator, Terminator::Return);
        assert!(early_return.normal_succs.is_empty());
        assert!(cfg.blocks[0].is_loop_header);
    }

    #[test]
    fn verifier_rejects_wrong_predecessor() {
        let mut cfg = graph(vec![(Op::Nop, vec![]), (Op::ReturnUndefined, vec![])]);
        cfg.blocks[0].preds.push(BlockId(0));

        assert_eq!(
            cfg.verify(),
            Err(CfgError::PredecessorMismatch { block: BlockId(0) })
        );
    }
}
