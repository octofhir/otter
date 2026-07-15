//! Backend-independent control-flow graph over one compiled unit.
//!
//! A compiled unit is the outermost function plus every callee body spliced
//! into it, as decided by [`crate::ir::inline`]. Each block therefore names the
//! frame it belongs to, and its PCs are canonical within that frame only.
//!
//! # Contents
//! - [`BlockId`] — dense block identity.
//! - [`Terminator`] and [`Block`] — typed block endings and complete edges.
//! - [`ControlFlowGraph`] — deterministic graph construction and verification.
//! - [`CfgError`] — precise construction and graph-integrity failures.
//!
//! # Invariants
//! - Block leaders, loop headers, and exception ranges come from the VM-owned
//!   [`otter_vm::CodeBlock`] control-flow table of the block's own frame.
//! - Normal successors come only from the authoritative opcode schema, except
//!   at a splice: a spliced call reaches its callee's entry, and the callee's
//!   returns reach the call's continuation block.
//! - Blocks are grouped by frame; within a frame they ascend by leader PC and
//!   cover every canonical PC exactly once.
//! - All edge lists are sorted, dense, and duplicate-free.
//! - Exception edges target the innermost enclosing catch or finally handler of
//!   the raising block's own frame.
//!
//! # See also
//! - [`crate::ir::inline`] — the splice decision this graph is built over.
//! - [`otter_bytecode::opcode_schema`]
//! - [`otter_vm::CodeBlockControlFlowView`]

use std::collections::{BTreeMap, BTreeSet};

use otter_bytecode::{
    Op,
    opcode_schema::{ControlFlow, SuccessorSpec, opcode_schema},
};
use otter_vm::{CodeBlock, JitCompileSnapshot, JitInstructionMetadata};
use smallvec::{SmallVec, smallvec};

use crate::ir::inline::{InlineId, InlineTree};

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
    /// A call whose callee body is spliced into this unit. Control enters the
    /// callee's entry block instead of leaving the unit; the callee's returns
    /// rejoin at [`continuation`](Self::InlineCall::continuation).
    InlineCall {
        /// Entry block of the spliced callee frame.
        callee_entry: BlockId,
        /// Block holding the caller's instructions after the call.
        continuation: BlockId,
    },
    /// A return from a spliced callee frame; control rejoins the caller.
    InlineReturn {
        /// Continuation block in the caller frame.
        continuation: BlockId,
    },
}

/// One basic block and all of its normal, exceptional, and incoming edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Dense block identity, equal to this block's graph index.
    pub id: BlockId,
    /// Frame this block's instructions and PCs belong to.
    pub inline: InlineId,
    /// Canonical instruction PC of the leader, within [`Self::inline`].
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
    /// Blocks grouped by frame and ascending by leader PC within each frame;
    /// index equals `BlockId.0`.
    pub blocks: Vec<Block>,
    /// Unit entry, always `BlockId(0)`: the root frame's first block.
    pub entry: BlockId,
    /// Entry block of each frame, indexed by [`InlineId`].
    pub frame_entries: Vec<BlockId>,
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
    /// Build the graph for one function with nothing spliced into it.
    pub fn build(view: &JitCompileSnapshot) -> Result<Self, CfgError> {
        Self::build_inlined(&InlineTree::trivial(view))
    }

    /// Build the graph for a whole compiled unit.
    ///
    /// Every frame's blocks are built from its own authoritative control-flow
    /// table, then the splices are wired: the block ending at a spliced call
    /// reaches the callee's entry, and each of the callee's returns reaches the
    /// call's continuation block. The caller's instructions after the call are
    /// forced into their own block by seeding the call's following PC as an
    /// extra leader, so a spliced call always ends a block.
    pub fn build_inlined(tree: &InlineTree) -> Result<Self, CfgError> {
        let mut frames = Vec::with_capacity(tree.frames.len());
        let mut base = 0u32;
        for frame in &tree.frames {
            // A spliced call must end its block so the continuation is a
            // distinct merge point for the callee's returns.
            let mut extra_leaders = BTreeSet::new();
            for other in &tree.frames {
                if let Some(call_site) = other.call_site.as_ref()
                    && call_site.parent == frame.id
                {
                    extra_leaders.insert(call_site.call_pc + 1);
                }
            }
            let built = FrameBlocks::build(
                frame.id,
                frame.code_block.as_ref(),
                &frame.instructions,
                &extra_leaders,
                base,
            )?;
            base += built.blocks.len() as u32;
            frames.push(built);
        }

        let frame_entries: Vec<BlockId> = frames
            .iter()
            .map(|frame| frame.blocks[0].id)
            .collect::<Vec<_>>();
        let mut blocks: Vec<Block> = frames
            .iter()
            .flat_map(|frame| frame.blocks.iter().cloned())
            .collect();

        for frame in &tree.frames {
            let Some(call_site) = frame.call_site.as_ref() else {
                continue;
            };
            let parent = &frames[call_site.parent.0 as usize];
            let call_block = parent.block_ending_at(call_site.call_pc).ok_or(
                CfgError::MissingTargetBlock {
                    target_pc: call_site.call_pc,
                },
            )?;
            let continuation = parent.block_starting_at(call_site.call_pc + 1).ok_or(
                CfgError::MissingTargetBlock {
                    target_pc: call_site.call_pc + 1,
                },
            )?;
            let callee_entry = frame_entries[frame.id.0 as usize];

            let block = &mut blocks[call_block.0 as usize];
            block.terminator = Terminator::InlineCall {
                callee_entry,
                continuation,
            };
            block.normal_succs = smallvec![callee_entry];

            for block in &mut blocks {
                if block.inline == frame.id && block.terminator == Terminator::Return {
                    block.terminator = Terminator::InlineReturn { continuation };
                    block.normal_succs = smallvec![continuation];
                }
            }
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
            frame_entries,
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
        if self.blocks[0].start_pc != 0 || self.blocks[0].inline != InlineId::ROOT {
            return Err(CfgError::EntryStartMismatch {
                expected: 0,
                actual: self.blocks[0].start_pc,
            });
        }
        if self.frame_entries.first() != Some(&self.entry) {
            return Err(CfgError::EntryNotZero { entry: self.entry });
        }
        for (index, &entry) in self.frame_entries.iter().enumerate() {
            let block = self
                .blocks
                .get(entry.0 as usize)
                .ok_or(CfgError::EdgeOutOfRange {
                    block: entry,
                    edge: entry,
                })?;
            if block.inline != InlineId(index as u32) || block.start_pc != 0 {
                return Err(CfgError::EntryStartMismatch {
                    expected: 0,
                    actual: block.start_pc,
                });
            }
        }

        let block_count = self.blocks.len();
        // Each frame owns a private PC space, so coverage and ordering restart
        // at every frame boundary. Frames themselves stay grouped and ascending.
        let mut expected_pc = 0;
        for (index, block) in self.blocks.iter().enumerate() {
            let expected_id = BlockId(index as u32);
            if block.id != expected_id {
                return Err(CfgError::BlockIdMismatch {
                    expected: expected_id,
                    actual: block.id,
                });
            }
            if index > 0 {
                let previous = &self.blocks[index - 1];
                if previous.inline > block.inline {
                    return Err(CfgError::BlockOrder {
                        previous: previous.id,
                        current: block.id,
                    });
                }
                if previous.inline == block.inline && previous.start_pc >= block.start_pc {
                    return Err(CfgError::BlockOrder {
                        previous: previous.id,
                        current: block.id,
                    });
                }
                if previous.inline != block.inline {
                    expected_pc = 0;
                }
            }
            let Some(&first_pc) = block.instr_pcs.first() else {
                return Err(CfgError::EmptyBlock { block: block.id });
            };
            if first_pc != block.start_pc {
                return Err(CfgError::LeaderSetMismatch);
            }
            // PCs ascend strictly and never overlap. Coverage gaps between
            // blocks are legitimate: construction prunes unreachable blocks, so
            // a gap is exactly where dead bytecode was. Within a block PCs are
            // contiguous by construction (a block spans leader to leader).
            let mut block_expected = block.start_pc;
            for &pc in &block.instr_pcs {
                if pc < expected_pc || pc != block_expected {
                    return Err(CfgError::InstructionOverlap { pc });
                }
                block_expected = block_expected
                    .checked_add(1)
                    .ok_or(CfgError::InstructionOverlap { pc })?;
                expected_pc = pc
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
            // A back edge is a same-frame edge to a leader at or before the
            // predecessor. A callee's return edge leaves its frame and is never
            // a back edge, whatever the two PCs compare as.
            let has_back_edge = self.blocks.iter().any(|pred| {
                pred.inline == block.inline
                    && pred.start_pc >= block.start_pc
                    && pred.normal_succs.contains(&block.id)
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

/// Blocks of one frame, built from that frame's own control-flow table.
struct FrameBlocks {
    blocks: Vec<Block>,
    /// Leader PC → global block id, for this frame only.
    block_by_pc: BTreeMap<u32, BlockId>,
}

impl FrameBlocks {
    fn build(
        inline: InlineId,
        code_block: &CodeBlock,
        instructions: &[JitInstructionMetadata],
        extra_leaders: &BTreeSet<u32>,
        base: u32,
    ) -> Result<Self, CfgError> {
        let instruction_count =
            u32::try_from(instructions.len()).map_err(|_| CfgError::InvalidSuccessorPc {
                pc: u32::MAX,
                target: i64::MAX,
            })?;
        if instruction_count == 0 {
            return Err(CfgError::EmptyFunction);
        }
        for (index, instruction) in instructions.iter().enumerate() {
            let index = index as u32;
            let pc = instruction.instruction_pc(code_block);
            if pc != index {
                return Err(CfgError::InstructionMetadataMismatch { index, pc });
            }
        }

        let control_flow = code_block.control_flow();
        let mut leader_set: BTreeSet<u32> = control_flow.block_starts().iter().copied().collect();
        for &leader in extra_leaders {
            if leader >= instruction_count {
                return Err(CfgError::InvalidSuccessorPc {
                    pc: leader,
                    target: i64::from(leader),
                });
            }
            leader_set.insert(leader);
        }
        let leaders: Vec<u32> = leader_set.into_iter().collect();
        if leaders.first() != Some(&0) {
            return Err(CfgError::LeaderSetMismatch);
        }
        let mut block_by_pc = BTreeMap::new();
        for (index, &pc) in leaders.iter().enumerate() {
            block_by_pc.insert(pc, BlockId(base + index as u32));
        }

        let mut blocks = Vec::with_capacity(leaders.len());
        for (index, &start_pc) in leaders.iter().enumerate() {
            let end_pc = leaders.get(index + 1).copied().unwrap_or(instruction_count);
            if start_pc >= end_pc || end_pc > instruction_count {
                return Err(CfgError::LeaderSetMismatch);
            }
            let id = BlockId(base + index as u32);
            let last_pc = end_pc - 1;
            let last_instruction = &instructions[last_pc as usize];
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
                !opcode_schema(instructions[pc as usize].op(code_block))
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
                inline,
                start_pc,
                instr_pcs: (start_pc..end_pc).collect(),
                terminator,
                normal_succs,
                exception_succs,
                preds: SmallVec::new(),
                is_loop_header: control_flow.loop_headers().binary_search(&start_pc).is_ok(),
            });
        }

        // Real bytecode carries unreachable blocks — code after a return in a
        // branch, a leader behind an unconditional jump. They never execute, so
        // no analysis may see them: prune by reachability from the frame entry
        // and renumber densely. PC coverage then legitimately has gaps exactly
        // where dead code was.
        let mut reachable = vec![false; blocks.len()];
        let mut pending = vec![0_usize];
        while let Some(index) = pending.pop() {
            if std::mem::replace(&mut reachable[index], true) {
                continue;
            }
            pending.extend(
                blocks[index]
                    .normal_succs
                    .iter()
                    .chain(&blocks[index].exception_succs)
                    .map(|successor| (successor.0 - base) as usize),
            );
        }
        if reachable.iter().any(|&kept| !kept) {
            let mut remap = vec![None; blocks.len()];
            let mut next = 0_u32;
            for (index, &kept) in reachable.iter().enumerate() {
                if kept {
                    remap[index] = Some(BlockId(base + next));
                    next += 1;
                }
            }
            let remap_id = |id: BlockId| {
                remap[(id.0 - base) as usize].expect("a reachable block only targets reachable")
            };
            let mut kept_blocks = Vec::with_capacity(next as usize);
            for (index, mut block) in blocks.into_iter().enumerate() {
                if !reachable[index] {
                    continue;
                }
                block.id = remap_id(block.id);
                for successor in &mut block.normal_succs {
                    *successor = remap_id(*successor);
                }
                for successor in &mut block.exception_succs {
                    *successor = remap_id(*successor);
                }
                if let Terminator::Branch { taken, fallthrough } = &mut block.terminator {
                    *taken = remap_id(*taken);
                    *fallthrough = remap_id(*fallthrough);
                }
                kept_blocks.push(block);
            }
            blocks = kept_blocks;
            block_by_pc = blocks
                .iter()
                .map(|block| (block.start_pc, block.id))
                .collect();
            // The VM's loop-header table counted dead back edges too; a header
            // whose only latch was pruned is a plain block now. Recompute from
            // the surviving edges with the verifier's own definition.
            let mut is_header = vec![false; blocks.len()];
            for block in &blocks {
                for successor in block.normal_succs.iter().copied() {
                    let target = &blocks[(successor.0 - base) as usize];
                    if target.start_pc <= block.start_pc {
                        is_header[(successor.0 - base) as usize] = true;
                    }
                }
            }
            for (index, block) in blocks.iter_mut().enumerate() {
                block.is_loop_header = is_header[index];
            }
        }

        Ok(Self {
            blocks,
            block_by_pc,
        })
    }

    fn block_starting_at(&self, pc: u32) -> Option<BlockId> {
        self.block_by_pc.get(&pc).copied()
    }

    fn block_ending_at(&self, pc: u32) -> Option<BlockId> {
        self.blocks
            .iter()
            .find(|block| block.instr_pcs.last() == Some(&pc))
            .map(|block| block.id)
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
        // A splice replaces the schema's fallthrough edge: the call reaches its
        // callee, and the callee's returns reach the continuation. Neither is a
        // schema successor, so both are checked against the splice itself.
        Terminator::InlineCall {
            callee_entry,
            continuation,
        } => {
            if callee_entry.0 as usize >= graph.blocks.len()
                || continuation.0 as usize >= graph.blocks.len()
            {
                return Err(CfgError::EdgeOutOfRange {
                    block: block.id,
                    edge: callee_entry,
                });
            }
            if block.normal_succs.as_slice() != [callee_entry] {
                return Err(CfgError::TerminatorMismatch { block: block.id });
            }
            let callee = &graph.blocks[callee_entry.0 as usize];
            if callee.inline == block.inline || callee.start_pc != 0 {
                return Err(CfgError::TerminatorMismatch { block: block.id });
            }
            if graph.blocks[continuation.0 as usize].inline != block.inline {
                return Err(CfgError::TerminatorMismatch { block: block.id });
            }
        }
        Terminator::InlineReturn { continuation } => {
            if continuation.0 as usize >= graph.blocks.len() {
                return Err(CfgError::EdgeOutOfRange {
                    block: block.id,
                    edge: continuation,
                });
            }
            if block.normal_succs.as_slice() != [continuation] {
                return Err(CfgError::TerminatorMismatch { block: block.id });
            }
            if graph.blocks[continuation.0 as usize].inline == block.inline {
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

    /// A one-parameter callee whose body is `return r0`.
    fn inline_callee(fid: u32) -> otter_vm::JitInlineCallee {
        let view = JitCompileSnapshot::without_feedback(
            fid,
            1,
            8,
            vec![JitTestInstruction::new(
                Op::ReturnValue,
                0,
                0,
                vec![Operand::Register(0)],
            )],
        );
        otter_vm::JitInlineCallee {
            code_block: std::sync::Arc::clone(&view.code_block),
            function_id: fid,
            param_count: 1,
            register_count: view.code_block.register_count,
            instructions: view.instructions,
        }
    }

    /// `r0 = r1(r2)` then `return r0`, with the call spliced.
    fn spliced_graph() -> ControlFlowGraph {
        let mut view = snapshot(vec![
            (
                Op::Call,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        let call_byte_pc = view.instructions[0].byte_pc;
        view.inline_callees.insert(call_byte_pc, inline_callee(9));
        let tree = InlineTree::build(&view);
        tree.verify().expect("the tree verifies");
        assert_eq!(tree.frames.len(), 2, "the fixture must splice");
        let graph = ControlFlowGraph::build_inlined(&tree).expect("a spliced CFG builds");
        graph.verify().expect("a spliced CFG verifies");
        graph
    }

    #[test]
    fn unreachable_blocks_are_pruned_not_rejected() {
        // `return r0` at pc1 makes pc2 (a jump back to a dead loop) and pc3
        // unreachable — the shape a branch-with-early-return leaves behind.
        let graph = graph(vec![
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::ReturnValue, vec![Operand::Register(0)]),
            (Op::Jump, vec![Operand::Imm32(-2)]),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);

        // Only the entry and the reachable return remain; the PC space keeps a
        // gap exactly where the dead code was.
        assert_eq!(graph.blocks.len(), 2);
        assert_eq!(graph.blocks[0].instr_pcs, vec![0]);
        assert_eq!(graph.blocks[1].instr_pcs, vec![1]);
        assert_eq!(graph.blocks[1].terminator, Terminator::Return);
        assert!(graph.blocks.iter().all(|block| !block.is_loop_header));
    }

    #[test]
    fn splice_ends_the_call_block_and_rejoins_at_the_continuation() {
        let graph = spliced_graph();

        // The caller splits at the call; the callee contributes its own block.
        assert_eq!(graph.blocks.len(), 3);
        assert_eq!(graph.frame_entries, vec![BlockId(0), BlockId(2)]);

        let call_block = &graph.blocks[0];
        assert_eq!(call_block.inline, InlineId::ROOT);
        assert_eq!(call_block.instr_pcs, vec![0]);
        assert_eq!(
            call_block.terminator,
            Terminator::InlineCall {
                callee_entry: BlockId(2),
                continuation: BlockId(1),
            }
        );
        assert_eq!(call_block.normal_succs.as_slice(), [BlockId(2)]);

        let continuation = &graph.blocks[1];
        assert_eq!(continuation.inline, InlineId::ROOT);
        assert_eq!(continuation.instr_pcs, vec![1]);
        // The caller no longer falls through into its own continuation: the
        // callee's return is its only predecessor.
        assert_eq!(continuation.preds.as_slice(), [BlockId(2)]);

        let callee = &graph.blocks[2];
        assert_eq!(callee.inline, InlineId(1));
        assert_eq!(callee.start_pc, 0, "each frame owns a private PC space");
        assert_eq!(
            callee.terminator,
            Terminator::InlineReturn {
                continuation: BlockId(1),
            }
        );
        assert_eq!(callee.preds.as_slice(), [BlockId(0)]);
    }

    #[test]
    fn a_callee_return_edge_is_not_a_back_edge() {
        // The callee's leader PC (0) is at or before the call block's leader PC
        // (0), so a frame-blind back-edge test would call the continuation a
        // loop header. It is not one.
        let graph = spliced_graph();
        assert!(graph.blocks.iter().all(|block| !block.is_loop_header));
    }

    #[test]
    fn verify_rejects_a_splice_that_returns_into_its_own_frame() {
        let mut graph = spliced_graph();
        // Point the callee's return at a block in its own frame.
        graph.blocks[2].terminator = Terminator::InlineReturn {
            continuation: BlockId(2),
        };
        graph.blocks[2].normal_succs = smallvec![BlockId(2)];
        assert!(graph.verify().is_err());
    }

    #[test]
    fn build_uses_a_trivial_tree_and_leaves_the_call_alone() {
        // The same body without a baked candidate keeps one frame and the
        // call's schema fallthrough.
        let graph = graph(vec![
            (
                Op::Call,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        assert_eq!(graph.frame_entries, vec![BlockId(0)]);
        assert!(graph.blocks.iter().all(|block| block.inline == InlineId::ROOT));
        // Nothing splits the body: the call keeps its schema fallthrough inside
        // one block that ends at the return.
        assert_eq!(graph.blocks.len(), 1);
        assert_eq!(graph.blocks[0].instr_pcs, vec![0, 1]);
        assert_eq!(graph.blocks[0].terminator, Terminator::Return);
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
