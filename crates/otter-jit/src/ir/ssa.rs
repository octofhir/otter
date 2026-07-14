//! Deterministic Cytron SSA construction over bytecode virtual registers.
//!
//! # Contents
//! - [`SsaFunction`] — block/value storage and SSA construction.
//! - [`ValueDef`] and [`SsaInstr`] — value definitions and renamed bytecode.
//! - [`SsaError`] — precise construction and pure verification failures.
//!
//! # Invariants
//! - Phi placement and renaming use normal edges and the normal-edge dominator
//!   forest; exception edges never supply phi inputs.
//! - Every exception-handler entry reloads every virtual register through a
//!   fresh [`ValueDef::ExceptionInput`].
//! - Verification checks use-def dominance against the full-edge dominator
//!   tree, including same-block definition order.
//! - Operand reads and writes come only from the authoritative bytecode operand
//!   schema; this module owns no opcode classification table.
//! - Values are dense and deterministic in normal-edge block RPO, with block
//!   head definitions before bytecode results.
//!
//! # See also
//! - [`crate::ir::cfg`]
//! - [`crate::ir::dom`]
//! - [`otter_bytecode::opcode_schema`]

use std::collections::{BTreeSet, VecDeque};

use otter_bytecode::{
    Op, Operand,
    opcode_schema::{
        OperandKind, OperandShape, OperandSpec, RegisterAccess, RegisterSource, opcode_schema,
    },
};
use otter_vm::JitCompileSnapshot;
use smallvec::SmallVec;

use super::{
    cfg::{BlockId, ControlFlowGraph},
    dom::{DomError, DominanceFrontier, DominatorTree},
};

/// Dense SSA value identity; every value is defined exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueId(pub u32);

/// The unique definition that produces one SSA value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueDef {
    /// Incoming `this` or parameter register.
    Param {
        /// Seeded bytecode register.
        register: u16,
        /// Incoming parameter index.
        index: u32,
    },
    /// Entry value for a register not initialized by the call ABI.
    Uninitialized {
        /// Seeded bytecode register.
        register: u16,
    },
    /// Register frame reload at an exception-handler entry.
    ExceptionInput {
        /// Handler block performing the reload.
        block: BlockId,
        /// Reloaded bytecode register.
        register: u16,
    },
    /// Normal-edge phi definition.
    Phi {
        /// Block containing the phi.
        block: BlockId,
        /// Bytecode register merged by the phi.
        register: u16,
        /// One value per normal predecessor, in predecessor order.
        inputs: Box<[ValueId]>,
    },
    /// Result of one bytecode instruction.
    Op {
        /// Original canonical bytecode PC.
        pc: u32,
        /// Original bytecode opcode.
        op: Op,
        /// SSA values for schema-declared read operands, in operand order.
        inputs: Box<[ValueId]>,
    },
}

/// Stored metadata for one dense SSA value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueData {
    /// Dense identity equal to this value's index in [`SsaFunction::values`].
    pub id: ValueId,
    /// Unique value definition.
    pub def: ValueDef,
    /// Block containing the definition.
    pub def_block: BlockId,
}

/// One bytecode instruction with register reads renamed to SSA values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsaInstr {
    /// Original canonical bytecode PC.
    pub pc: u32,
    /// Original bytecode opcode.
    pub op: Op,
    /// SSA values for schema-declared read registers, in operand order.
    pub inputs: SmallVec<[ValueId; 4]>,
    /// SSA result when the instruction writes one register.
    pub result: Option<ValueId>,
}

/// SSA contents attached to one CFG block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsaBlock {
    /// Dense identity equal to this block's index in [`SsaFunction::blocks`].
    pub id: BlockId,
    /// Phi, entry-seed, or exception-input values defined at block entry.
    pub phis: Vec<ValueId>,
    /// Renamed bytecode instructions in canonical PC order.
    pub instrs: Vec<SsaInstr>,
}

/// Complete SSA form for one bytecode function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsaFunction {
    /// Blocks indexed by [`BlockId`], matching the source CFG.
    pub blocks: Vec<SsaBlock>,
    /// Dense values indexed by [`ValueId`].
    pub values: Vec<ValueData>,
    /// Function entry block.
    pub entry: BlockId,
    /// Number of bytecode virtual registers represented by the SSA graph.
    pub register_count: u16,
}

/// Failure to construct or verify SSA form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SsaError {
    /// Snapshot instruction metadata does not cover the CFG instruction PCs.
    SnapshotInstructionCountMismatch {
        /// Number of snapshot instructions.
        snapshot: usize,
        /// Number of CFG instructions.
        cfg: usize,
    },
    /// Snapshot metadata reports a non-canonical instruction PC.
    InstructionPcMismatch {
        /// Expected canonical PC.
        expected: u32,
        /// PC reported by the snapshot metadata.
        actual: u32,
    },
    /// An authoritative operand is absent or has the wrong wire kind.
    InvalidOperand {
        /// Instruction owning the invalid operand.
        pc: u32,
        /// Operand position.
        operand_index: usize,
        /// Schema-declared operand kind.
        expected: OperandKind,
    },
    /// A schema-declared register index cannot be represented as `u16`.
    InvalidRegisterEncoding {
        /// Instruction owning the register operand.
        pc: u32,
        /// Operand position.
        operand_index: usize,
    },
    /// A schema-declared register lies outside the function register window.
    RegisterOutOfRange {
        /// Instruction PC, or `None` for a stored head definition.
        pc: Option<u32>,
        /// Invalid register index.
        register: u16,
        /// Function register count.
        register_count: u16,
    },
    /// The pinned single-result instruction shape cannot represent this opcode's writes.
    MultipleRegisterDefinitions {
        /// Instruction owning both writes.
        pc: u32,
        /// First schema-declared destination register.
        first: u16,
        /// Second schema-declared destination register.
        second: u16,
    },
    /// The function contains more values than [`ValueId`] can represent.
    ValueIdOverflow,
    /// Renaming found no reaching definition for a schema-declared register use.
    MissingReachingDefinition {
        /// Block containing the use.
        block: BlockId,
        /// Instruction PC, or `None` while filling a successor phi.
        pc: Option<u32>,
        /// Register lacking a reaching definition.
        register: u16,
    },
    /// A normal successor does not list the current block as a normal predecessor.
    MissingNormalPredecessor {
        /// Successor containing the phi.
        block: BlockId,
        /// Missing predecessor.
        predecessor: BlockId,
    },
    /// SSA block storage does not match the CFG block set.
    BlockCountMismatch {
        /// Number of CFG blocks.
        expected: usize,
        /// Number of SSA blocks.
        actual: usize,
    },
    /// An SSA block's stored identity differs from its dense index.
    BlockIdMismatch {
        /// Identity implied by the dense index.
        expected: BlockId,
        /// Stored identity.
        actual: BlockId,
    },
    /// SSA and CFG entry identities differ.
    EntryMismatch {
        /// CFG entry.
        expected: BlockId,
        /// SSA entry.
        actual: BlockId,
    },
    /// The supplied dominance tree does not include exception edges.
    NormalDominatorUsedForVerification,
    /// The supplied full-edge dominance tree is internally invalid.
    InvalidFullDominator(DomError),
    /// Dense value storage contains a mismatched identity.
    DenseValueIdMismatch {
        /// Identity implied by the dense index.
        expected: ValueId,
        /// Stored identity.
        actual: ValueId,
    },
    /// A structural definition references a value outside dense storage.
    ValueReferenceOutOfRange {
        /// Invalid value identity.
        value: ValueId,
    },
    /// One value is placed as a definition more than once.
    SingleAssignmentViolation {
        /// Multiply placed value.
        value: ValueId,
    },
    /// A stored value has no structural definition site.
    MissingDefinitionSite {
        /// Unplaced value.
        value: ValueId,
    },
    /// A value's stored defining block differs from its structural site.
    DefinitionBlockMismatch {
        /// Value with the mismatched block.
        value: ValueId,
        /// Structural definition block.
        expected: BlockId,
        /// Stored definition block.
        actual: BlockId,
    },
    /// A block-head value has a definition kind invalid at that site.
    InvalidHeadDefinition {
        /// Invalid head value.
        value: ValueId,
        /// Block containing the value.
        block: BlockId,
    },
    /// An instruction result's stored operation does not match the instruction.
    OperationDefinitionMismatch {
        /// Mismatched result value.
        value: ValueId,
        /// Instruction containing the result.
        pc: u32,
    },
    /// SSA instruction PCs do not match their CFG block's instruction list.
    InstructionLayoutMismatch {
        /// Block with the mismatched instruction layout.
        block: BlockId,
    },
    /// A phi does not have exactly one input per normal predecessor.
    PhiInputCountMismatch {
        /// Phi value.
        value: ValueId,
        /// Required input count.
        expected: usize,
        /// Stored input count.
        actual: usize,
    },
    /// A phi occurs at a block with fewer than two normal predecessors.
    PhiWithoutNormalJoin {
        /// Invalid phi value.
        value: ValueId,
        /// Block containing the phi.
        block: BlockId,
    },
    /// An exception-handler entry contains a normal-edge phi.
    PhiAtExceptionHandler {
        /// Invalid phi value.
        value: ValueId,
        /// Handler block.
        block: BlockId,
    },
    /// A handler lacks the required frame reload for one register.
    MissingExceptionInput {
        /// Handler block.
        block: BlockId,
        /// Missing register.
        register: u16,
    },
    /// A handler defines more than one frame reload for a register.
    DuplicateExceptionInput {
        /// Handler block.
        block: BlockId,
        /// Duplicated register.
        register: u16,
    },
    /// An exception-input value occurs outside an exception-handler entry.
    UnexpectedExceptionInput {
        /// Invalid exception-input value.
        value: ValueId,
        /// Non-handler block.
        block: BlockId,
    },
    /// Entry lacks its required parameter/uninitialized seed for one register.
    MissingEntryInput {
        /// Missing register.
        register: u16,
    },
    /// Entry contains duplicate parameter/uninitialized seeds for one register.
    DuplicateEntryInput {
        /// Duplicated register.
        register: u16,
    },
    /// A parameter seed's stable incoming index differs from its register.
    ParameterIndexMismatch {
        /// Parameter value.
        value: ValueId,
        /// Seeded register.
        register: u16,
        /// Stored incoming index.
        index: u32,
    },
    /// A use references a definition whose block does not dominate it.
    UseDefinitionDoesNotDominate {
        /// Block containing the use.
        block: BlockId,
        /// Used value.
        value: ValueId,
    },
    /// A same-block instruction use precedes its defining instruction.
    UseBeforeDefinition {
        /// Block containing both operations.
        block: BlockId,
        /// Instruction containing the invalid use.
        pc: u32,
        /// Used value.
        value: ValueId,
    },
    /// A phi input's definition does not dominate its normal predecessor edge.
    PhiInputDoesNotDominatePredecessor {
        /// Phi value.
        phi: ValueId,
        /// Input value.
        input: ValueId,
        /// Normal predecessor whose edge carries the input.
        predecessor: BlockId,
    },
    /// Value ids do not follow deterministic RPO/head/instruction order.
    NonDeterministicValueOrder {
        /// Value expected at this structural definition site.
        expected: ValueId,
        /// Value stored at the site.
        actual: ValueId,
    },
    /// Block-head definitions are not in canonical category/register order.
    NonCanonicalHeadOrder {
        /// Block containing the non-canonical head sequence.
        block: BlockId,
    },
}

impl std::fmt::Display for SsaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid SSA: {self:?}")
    }
}

impl std::error::Error for SsaError {}

#[derive(Debug, Clone, Default)]
struct RegisterFlow {
    uses: SmallVec<[u16; 4]>,
    def: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
enum DefinitionPosition {
    Head,
    Instruction(usize),
}

impl SsaFunction {
    /// Build deterministic Cytron SSA for a verified snapshot and CFG.
    pub fn build(view: &JitCompileSnapshot, cfg: &ControlFlowGraph) -> Result<Self, SsaError> {
        let instruction_count: usize = cfg.blocks.iter().map(|block| block.instr_pcs.len()).sum();
        if view.instructions.len() != instruction_count {
            return Err(SsaError::SnapshotInstructionCountMismatch {
                snapshot: view.instructions.len(),
                cfg: instruction_count,
            });
        }

        let code_block = view.code_block.as_ref();
        let register_count = code_block.register_count;
        let mut flows = vec![RegisterFlow::default(); instruction_count];
        let mut def_blocks = vec![BTreeSet::new(); usize::from(register_count)];
        for register_defs in &mut def_blocks {
            register_defs.insert(cfg.entry);
        }

        let handlers = exception_handlers(cfg);
        for (index, is_handler) in handlers.iter().copied().enumerate() {
            if is_handler {
                for register_defs in &mut def_blocks {
                    register_defs.insert(BlockId(index as u32));
                }
            }
        }

        for block in &cfg.blocks {
            for &pc in &block.instr_pcs {
                let instruction = &view.instructions[pc as usize];
                let actual_pc = instruction.instruction_pc(code_block);
                if actual_pc != pc {
                    return Err(SsaError::InstructionPcMismatch {
                        expected: pc,
                        actual: actual_pc,
                    });
                }
                let flow = register_flow(view, pc, register_count)?;
                if let Some(register) = flow.def {
                    def_blocks[usize::from(register)].insert(block.id);
                }
                flows[pc as usize] = flow;
            }
        }

        let normal_dom = DominatorTree::compute_normal(cfg);
        let frontier = DominanceFrontier::compute(cfg, &normal_dom);
        let mut phi_registers = vec![BTreeSet::new(); cfg.blocks.len()];
        for register in 0..register_count {
            let definitions = &def_blocks[usize::from(register)];
            let mut worklist: VecDeque<_> = definitions.iter().copied().collect();
            while let Some(definition) = worklist.pop_front() {
                for &target in frontier.frontier(definition) {
                    let target_index = target.0 as usize;
                    if handlers[target_index] || normal_predecessors(cfg, target).len() < 2 {
                        continue;
                    }
                    if phi_registers[target_index].insert(register)
                        && !definitions.contains(&target)
                    {
                        worklist.push_back(target);
                    }
                }
            }
        }

        let mut blocks: Vec<_> = cfg
            .blocks
            .iter()
            .map(|block| SsaBlock {
                id: block.id,
                phis: Vec::new(),
                instrs: Vec::with_capacity(block.instr_pcs.len()),
            })
            .collect();
        let mut values = Vec::new();
        for &block_id in normal_dom.reverse_postorder() {
            let block_index = block_id.0 as usize;
            if handlers[block_index] {
                for register in 0..register_count {
                    let id = append_value(
                        &mut values,
                        ValueDef::ExceptionInput {
                            block: block_id,
                            register,
                        },
                        block_id,
                    )?;
                    blocks[block_index].phis.push(id);
                }
            } else if block_id == cfg.entry {
                for register in 0..register_count {
                    let def = if register < code_block.param_count {
                        ValueDef::Param {
                            register,
                            index: u32::from(register),
                        }
                    } else {
                        ValueDef::Uninitialized { register }
                    };
                    let id = append_value(&mut values, def, block_id)?;
                    blocks[block_index].phis.push(id);
                }
            }

            if !handlers[block_index] {
                let predecessor_count = normal_predecessors(cfg, block_id).len();
                for &register in &phi_registers[block_index] {
                    let id = append_value(
                        &mut values,
                        ValueDef::Phi {
                            block: block_id,
                            register,
                            inputs: vec![ValueId(u32::MAX); predecessor_count].into_boxed_slice(),
                        },
                        block_id,
                    )?;
                    blocks[block_index].phis.push(id);
                }
            }

            for &pc in &cfg.blocks[block_index].instr_pcs {
                let instruction = &view.instructions[pc as usize];
                let op = instruction.op(code_block);
                let result = if flows[pc as usize].def.is_some() {
                    Some(append_value(
                        &mut values,
                        ValueDef::Op {
                            pc,
                            op,
                            inputs: Box::new([]),
                        },
                        block_id,
                    )?)
                } else {
                    None
                };
                blocks[block_index].instrs.push(SsaInstr {
                    pc,
                    op,
                    inputs: SmallVec::new(),
                    result,
                });
            }
        }

        let mut function = Self {
            blocks,
            values,
            entry: cfg.entry,
            register_count,
        };
        function.rename(cfg, &normal_dom, &flows)?;
        let full_dom = DominatorTree::compute(cfg);
        function.verify(cfg, &full_dom)?;
        Ok(function)
    }

    fn rename(
        &mut self,
        cfg: &ControlFlowGraph,
        normal_dom: &DominatorTree,
        flows: &[RegisterFlow],
    ) -> Result<(), SsaError> {
        enum Event {
            Enter(BlockId),
            Exit(Vec<u16>),
        }

        let mut children = vec![Vec::new(); cfg.blocks.len()];
        for &block in normal_dom.reverse_postorder() {
            if let Some(parent) = normal_dom.immediate_dominator(block) {
                children[parent.0 as usize].push(block);
            }
        }
        let roots: Vec<_> = normal_dom
            .reverse_postorder()
            .iter()
            .copied()
            .filter(|&block| normal_dom.immediate_dominator(block).is_none())
            .collect();
        let mut stacks = vec![Vec::new(); usize::from(self.register_count)];

        for root in roots {
            let mut events = vec![Event::Enter(root)];
            while let Some(event) = events.pop() {
                match event {
                    Event::Exit(pushed) => {
                        for register in pushed.into_iter().rev() {
                            stacks[usize::from(register)]
                                .pop()
                                .expect("rename pops exactly the values it pushed");
                        }
                    }
                    Event::Enter(block_id) => {
                        let block_index = block_id.0 as usize;
                        let mut pushed = Vec::new();
                        for &value in &self.blocks[block_index].phis {
                            let register = head_register(&self.values[value.0 as usize].def)
                                .expect("construction puts only head values in SsaBlock::phis");
                            stacks[usize::from(register)].push(value);
                            pushed.push(register);
                        }

                        for instruction_index in 0..self.blocks[block_index].instrs.len() {
                            let pc = self.blocks[block_index].instrs[instruction_index].pc;
                            let flow = &flows[pc as usize];
                            let mut inputs = SmallVec::<[ValueId; 4]>::new();
                            for &register in &flow.uses {
                                let value = stacks[usize::from(register)].last().copied().ok_or(
                                    SsaError::MissingReachingDefinition {
                                        block: block_id,
                                        pc: Some(pc),
                                        register,
                                    },
                                )?;
                                inputs.push(value);
                            }
                            let result = self.blocks[block_index].instrs[instruction_index].result;
                            self.blocks[block_index].instrs[instruction_index].inputs =
                                inputs.clone();
                            if let Some(value) = result {
                                let ValueDef::Op {
                                    inputs: value_inputs,
                                    ..
                                } = &mut self.values[value.0 as usize].def
                                else {
                                    unreachable!(
                                        "construction records instruction results as Op values"
                                    );
                                };
                                *value_inputs = inputs.into_vec().into_boxed_slice();
                                let register = flow.def.expect(
                                    "construction creates results exactly for register defs",
                                );
                                stacks[usize::from(register)].push(value);
                                pushed.push(register);
                            }
                        }

                        for &successor in &cfg.blocks[block_index].normal_succs {
                            let predecessors = normal_predecessors(cfg, successor);
                            let predecessor_index = predecessors
                                .iter()
                                .position(|&predecessor| predecessor == block_id)
                                .ok_or(SsaError::MissingNormalPredecessor {
                                    block: successor,
                                    predecessor: block_id,
                                })?;
                            let successor_heads = self.blocks[successor.0 as usize].phis.clone();
                            for phi in successor_heads {
                                let register = match self.values[phi.0 as usize].def {
                                    ValueDef::Phi { register, .. } => register,
                                    _ => continue,
                                };
                                let input = stacks[usize::from(register)].last().copied().ok_or(
                                    SsaError::MissingReachingDefinition {
                                        block: successor,
                                        pc: None,
                                        register,
                                    },
                                )?;
                                let ValueDef::Phi { inputs, .. } =
                                    &mut self.values[phi.0 as usize].def
                                else {
                                    unreachable!();
                                };
                                inputs[predecessor_index] = input;
                            }
                        }

                        events.push(Event::Exit(pushed));
                        for &child in children[block_index].iter().rev() {
                            events.push(Event::Enter(child));
                        }
                    }
                }
            }
            debug_assert!(stacks.iter().all(Vec::is_empty));
        }
        Ok(())
    }

    /// Verify SSA structure, dominance, exception inputs, and deterministic order.
    pub fn verify(&self, cfg: &ControlFlowGraph, full_dom: &DominatorTree) -> Result<(), SsaError> {
        if !full_dom.includes_exception_edges() {
            return Err(SsaError::NormalDominatorUsedForVerification);
        }
        full_dom
            .verify(cfg)
            .map_err(SsaError::InvalidFullDominator)?;
        if self.blocks.len() != cfg.blocks.len() {
            return Err(SsaError::BlockCountMismatch {
                expected: cfg.blocks.len(),
                actual: self.blocks.len(),
            });
        }
        if self.entry != cfg.entry {
            return Err(SsaError::EntryMismatch {
                expected: cfg.entry,
                actual: self.entry,
            });
        }
        for (index, block) in self.blocks.iter().enumerate() {
            let expected = BlockId(index as u32);
            if block.id != expected {
                return Err(SsaError::BlockIdMismatch {
                    expected,
                    actual: block.id,
                });
            }
        }
        for (index, value) in self.values.iter().enumerate() {
            let expected = ValueId(index as u32);
            if value.id != expected {
                return Err(SsaError::DenseValueIdMismatch {
                    expected,
                    actual: value.id,
                });
            }
        }

        let handlers = exception_handlers(cfg);
        let mut placements = vec![None; self.values.len()];
        let mut entry_inputs = vec![false; usize::from(self.register_count)];
        let mut handler_inputs =
            vec![vec![false; usize::from(self.register_count)]; cfg.blocks.len()];

        for (block_index, block) in self.blocks.iter().enumerate() {
            let block_id = block.id;
            let normal_predecessors = normal_predecessors(cfg, block_id);
            let mut previous_head_key = None;
            for &value_id in &block.phis {
                let value = self.value(value_id)?;
                record_placement(&mut placements, value_id, DefinitionPosition::Head)?;
                if value.def_block != block_id {
                    return Err(SsaError::DefinitionBlockMismatch {
                        value: value_id,
                        expected: block_id,
                        actual: value.def_block,
                    });
                }
                let (category, register) = match &value.def {
                    ValueDef::Param { register, index } if block_id == self.entry => {
                        if *index != u32::from(*register) {
                            return Err(SsaError::ParameterIndexMismatch {
                                value: value_id,
                                register: *register,
                                index: *index,
                            });
                        }
                        mark_entry_input(&mut entry_inputs, *register, self.register_count)?;
                        (0_u8, *register)
                    }
                    ValueDef::Uninitialized { register } if block_id == self.entry => {
                        mark_entry_input(&mut entry_inputs, *register, self.register_count)?;
                        (0, *register)
                    }
                    ValueDef::ExceptionInput {
                        block: owner,
                        register,
                    } if *owner == block_id && handlers[block_index] => {
                        mark_handler_input(
                            &mut handler_inputs[block_index],
                            block_id,
                            *register,
                            self.register_count,
                        )?;
                        (0, *register)
                    }
                    ValueDef::ExceptionInput { .. } => {
                        return Err(SsaError::UnexpectedExceptionInput {
                            value: value_id,
                            block: block_id,
                        });
                    }
                    ValueDef::Phi {
                        block: owner,
                        register,
                        inputs,
                    } if *owner == block_id => {
                        if handlers[block_index] {
                            return Err(SsaError::PhiAtExceptionHandler {
                                value: value_id,
                                block: block_id,
                            });
                        }
                        if normal_predecessors.len() < 2 {
                            return Err(SsaError::PhiWithoutNormalJoin {
                                value: value_id,
                                block: block_id,
                            });
                        }
                        if inputs.len() != normal_predecessors.len() {
                            return Err(SsaError::PhiInputCountMismatch {
                                value: value_id,
                                expected: normal_predecessors.len(),
                                actual: inputs.len(),
                            });
                        }
                        (1, *register)
                    }
                    _ => {
                        return Err(SsaError::InvalidHeadDefinition {
                            value: value_id,
                            block: block_id,
                        });
                    }
                };
                check_register(None, register, self.register_count)?;
                let key = (category, register);
                if previous_head_key.is_some_and(|previous| previous >= key) {
                    return Err(SsaError::NonCanonicalHeadOrder { block: block_id });
                }
                previous_head_key = Some(key);
            }

            if block.instrs.len() != cfg.blocks[block_index].instr_pcs.len()
                || block
                    .instrs
                    .iter()
                    .map(|instruction| instruction.pc)
                    .ne(cfg.blocks[block_index].instr_pcs.iter().copied())
            {
                return Err(SsaError::InstructionLayoutMismatch { block: block_id });
            }
            for (instruction_index, instruction) in block.instrs.iter().enumerate() {
                if let Some(value_id) = instruction.result {
                    let value = self.value(value_id)?;
                    record_placement(
                        &mut placements,
                        value_id,
                        DefinitionPosition::Instruction(instruction_index),
                    )?;
                    if value.def_block != block_id {
                        return Err(SsaError::DefinitionBlockMismatch {
                            value: value_id,
                            expected: block_id,
                            actual: value.def_block,
                        });
                    }
                    match &value.def {
                        ValueDef::Op { pc, op, inputs }
                            if *pc == instruction.pc
                                && *op == instruction.op
                                && inputs.as_ref() == instruction.inputs.as_slice() => {}
                        _ => {
                            return Err(SsaError::OperationDefinitionMismatch {
                                value: value_id,
                                pc: instruction.pc,
                            });
                        }
                    }
                }
            }
        }

        for (index, placement) in placements.iter().enumerate() {
            if placement.is_none() {
                return Err(SsaError::MissingDefinitionSite {
                    value: ValueId(index as u32),
                });
            }
        }
        for register in 0..self.register_count {
            if !entry_inputs[usize::from(register)] && !handlers[self.entry.0 as usize] {
                return Err(SsaError::MissingEntryInput { register });
            }
        }
        for (block_index, is_handler) in handlers.iter().copied().enumerate() {
            if is_handler {
                for register in 0..self.register_count {
                    if !handler_inputs[block_index][usize::from(register)] {
                        return Err(SsaError::MissingExceptionInput {
                            block: BlockId(block_index as u32),
                            register,
                        });
                    }
                }
            }
        }

        for block in &self.blocks {
            for (instruction_index, instruction) in block.instrs.iter().enumerate() {
                for &input in &instruction.inputs {
                    let value = self.value(input)?;
                    if !full_dom.dominates(value.def_block, block.id) {
                        return Err(SsaError::UseDefinitionDoesNotDominate {
                            block: block.id,
                            value: input,
                        });
                    }
                    if value.def_block == block.id
                        && let Some(DefinitionPosition::Instruction(definition_index)) =
                            placements[input.0 as usize]
                        && definition_index >= instruction_index
                    {
                        return Err(SsaError::UseBeforeDefinition {
                            block: block.id,
                            pc: instruction.pc,
                            value: input,
                        });
                    }
                }
            }
            let predecessors = normal_predecessors(cfg, block.id);
            for &phi in &block.phis {
                let ValueDef::Phi { inputs, .. } = &self.value(phi)?.def else {
                    continue;
                };
                if inputs.len() != predecessors.len() {
                    return Err(SsaError::PhiInputCountMismatch {
                        value: phi,
                        expected: predecessors.len(),
                        actual: inputs.len(),
                    });
                }
                for (&input, &predecessor) in inputs.iter().zip(&predecessors) {
                    let input_value = self.value(input)?;
                    if !full_dom.dominates(input_value.def_block, predecessor) {
                        return Err(SsaError::PhiInputDoesNotDominatePredecessor {
                            phi,
                            input,
                            predecessor,
                        });
                    }
                }
            }
        }

        let normal_dom = DominatorTree::compute_normal(cfg);
        let mut next_value = 0_u32;
        for &block in normal_dom.reverse_postorder() {
            let ssa_block = &self.blocks[block.0 as usize];
            for &actual in &ssa_block.phis {
                let expected = ValueId(next_value);
                if actual != expected {
                    return Err(SsaError::NonDeterministicValueOrder { expected, actual });
                }
                next_value = next_value.checked_add(1).ok_or(SsaError::ValueIdOverflow)?;
            }
            for instruction in &ssa_block.instrs {
                if let Some(actual) = instruction.result {
                    let expected = ValueId(next_value);
                    if actual != expected {
                        return Err(SsaError::NonDeterministicValueOrder { expected, actual });
                    }
                    next_value = next_value.checked_add(1).ok_or(SsaError::ValueIdOverflow)?;
                }
            }
        }
        if next_value as usize != self.values.len() {
            return Err(SsaError::MissingDefinitionSite {
                value: ValueId(next_value),
            });
        }
        Ok(())
    }

    fn value(&self, id: ValueId) -> Result<&ValueData, SsaError> {
        self.values
            .get(id.0 as usize)
            .ok_or(SsaError::ValueReferenceOutOfRange { value: id })
    }
}

fn register_flow(
    view: &JitCompileSnapshot,
    pc: u32,
    register_count: u16,
) -> Result<RegisterFlow, SsaError> {
    let code_block = view.code_block.as_ref();
    let instruction = &view.instructions[pc as usize];
    let op = instruction.op(code_block);
    let operands = instruction.operand_view(code_block);
    let shape = opcode_schema(op).operand_shape;
    let mut flow = RegisterFlow::default();
    for operand_index in 0..operands.len() {
        let spec = operand_spec_at(shape, operand_index).expect("verified schema covers operands");
        if spec.register_access == RegisterAccess::None {
            continue;
        }
        let operand = operands
            .get(operand_index)
            .ok_or(SsaError::InvalidOperand {
                pc,
                operand_index,
                expected: spec.kind,
            })?;
        if OperandKind::of(&operand) != spec.kind {
            return Err(SsaError::InvalidOperand {
                pc,
                operand_index,
                expected: spec.kind,
            });
        }
        let register = match (spec.register_source, operand) {
            (Some(RegisterSource::RegisterOperand), Operand::Register(register)) => register,
            (Some(RegisterSource::Imm32RegisterIndex), Operand::Imm32(register)) => {
                u16::try_from(register)
                    .map_err(|_| SsaError::InvalidRegisterEncoding { pc, operand_index })?
            }
            _ => {
                return Err(SsaError::InvalidRegisterEncoding { pc, operand_index });
            }
        };
        check_register(Some(pc), register, register_count)?;
        match spec.register_access {
            RegisterAccess::Read => flow.uses.push(register),
            RegisterAccess::Write => {
                if let Some(first) = flow.def.replace(register) {
                    return Err(SsaError::MultipleRegisterDefinitions {
                        pc,
                        first,
                        second: register,
                    });
                }
            }
            RegisterAccess::None => unreachable!(),
        }
    }
    Ok(flow)
}

fn operand_spec_at(shape: OperandShape, index: usize) -> Option<OperandSpec> {
    let prefix = shape.prefix()?;
    if let Some(spec) = prefix.get(index) {
        return Some(*spec);
    }
    shape.variadic().map(|(_, tail)| tail)
}

fn append_value(
    values: &mut Vec<ValueData>,
    def: ValueDef,
    def_block: BlockId,
) -> Result<ValueId, SsaError> {
    let index = u32::try_from(values.len()).map_err(|_| SsaError::ValueIdOverflow)?;
    let id = ValueId(index);
    values.push(ValueData { id, def, def_block });
    Ok(id)
}

fn exception_handlers(cfg: &ControlFlowGraph) -> Vec<bool> {
    let mut handlers = vec![false; cfg.blocks.len()];
    for block in &cfg.blocks {
        for &handler in &block.exception_succs {
            handlers[handler.0 as usize] = true;
        }
    }
    handlers
}

fn normal_predecessors(cfg: &ControlFlowGraph, block: BlockId) -> SmallVec<[BlockId; 4]> {
    cfg.blocks[block.0 as usize]
        .preds
        .iter()
        .copied()
        .filter(|predecessor| {
            cfg.blocks[predecessor.0 as usize]
                .normal_succs
                .contains(&block)
        })
        .collect()
}

fn head_register(def: &ValueDef) -> Option<u16> {
    match def {
        ValueDef::Param { register, .. }
        | ValueDef::Uninitialized { register }
        | ValueDef::ExceptionInput { register, .. }
        | ValueDef::Phi { register, .. } => Some(*register),
        ValueDef::Op { .. } => None,
    }
}

fn check_register(pc: Option<u32>, register: u16, register_count: u16) -> Result<(), SsaError> {
    if register >= register_count {
        return Err(SsaError::RegisterOutOfRange {
            pc,
            register,
            register_count,
        });
    }
    Ok(())
}

fn record_placement(
    placements: &mut [Option<DefinitionPosition>],
    value: ValueId,
    position: DefinitionPosition,
) -> Result<(), SsaError> {
    let slot = placements
        .get_mut(value.0 as usize)
        .ok_or(SsaError::ValueReferenceOutOfRange { value })?;
    if slot.replace(position).is_some() {
        return Err(SsaError::SingleAssignmentViolation { value });
    }
    Ok(())
}

fn mark_entry_input(seen: &mut [bool], register: u16, register_count: u16) -> Result<(), SsaError> {
    check_register(None, register, register_count)?;
    if std::mem::replace(&mut seen[usize::from(register)], true) {
        return Err(SsaError::DuplicateEntryInput { register });
    }
    Ok(())
}

fn mark_handler_input(
    seen: &mut [bool],
    block: BlockId,
    register: u16,
    register_count: u16,
) -> Result<(), SsaError> {
    check_register(None, register, register_count)?;
    if std::mem::replace(&mut seen[usize::from(register)], true) {
        return Err(SsaError::DuplicateExceptionInput { block, register });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{NO_HANDLER_OFFSET, Operand};
    use otter_vm::jit::JitTestInstruction;

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

    fn build(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> (ControlFlowGraph, SsaFunction) {
        let snapshot = snapshot(param_count, register_count, instructions);
        let cfg = ControlFlowGraph::build(&snapshot).expect("CFG builds");
        let ssa = SsaFunction::build(&snapshot, &cfg).expect("SSA builds");
        ssa.verify(&cfg, &DominatorTree::compute(&cfg))
            .expect("SSA verifies");
        (cfg, ssa)
    }

    fn phi_for(ssa: &SsaFunction, block: BlockId, register: u16) -> Option<ValueId> {
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

    #[test]
    fn straight_line_uses_latest_register_definition() {
        let (_cfg, ssa) = build(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)]),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(2)]),
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

        assert!(
            !ssa.values
                .iter()
                .any(|value| matches!(value.def, ValueDef::Phi { .. }))
        );
        assert_ne!(op_value_at(&ssa, 0), op_value_at(&ssa, 1));
        let add = &ssa.blocks[0].instrs[2];
        assert_eq!(add.inputs[0], op_value_at(&ssa, 1));
        assert!(matches!(
            ssa.values[add.inputs[1].0 as usize].def,
            ValueDef::Param { register: 0, .. }
        ));
    }

    #[test]
    fn diamond_places_only_the_needed_phi_in_predecessor_order() {
        let (cfg, ssa) = build(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
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
        let join = cfg.blocks.iter().find(|block| block.start_pc == 5).unwrap();
        let phi = phi_for(&ssa, join.id, 1).expect("arm writes require a phi");
        let ValueDef::Phi { inputs, .. } = &ssa.values[phi.0 as usize].def else {
            unreachable!();
        };
        assert_eq!(
            inputs.as_ref(),
            &[op_value_at(&ssa, 2), op_value_at(&ssa, 4)]
        );
        assert!(phi_for(&ssa, join.id, 0).is_none());
        assert_eq!(ssa.blocks[join.id.0 as usize].instrs[0].inputs[0], phi);
    }

    #[test]
    fn while_loop_has_preheader_and_latch_phi_inputs() {
        let (cfg, ssa) = build(
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
        let header = cfg.blocks.iter().find(|block| block.start_pc == 2).unwrap();
        let phi = phi_for(&ssa, header.id, 1).expect("loop-carried value has a phi");
        let ValueDef::Phi { inputs, .. } = &ssa.values[phi.0 as usize].def else {
            unreachable!();
        };
        assert_eq!(
            inputs.as_ref(),
            &[op_value_at(&ssa, 0), op_value_at(&ssa, 3)]
        );
        assert_eq!(ssa.blocks[header.id.0 as usize].instrs[0].inputs[0], phi);
    }

    #[test]
    fn nested_loops_place_phis_at_both_headers() {
        let (cfg, ssa) = build(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(8), Operand::Register(0)],
                ),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(0)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(0),
                    ],
                ),
                (Op::Nop, vec![]),
                (Op::Jump, vec![Operand::Imm32(-4)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(1),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (Op::Nop, vec![]),
                (Op::Jump, vec![Operand::Imm32(-9)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let outer = cfg.blocks.iter().find(|block| block.start_pc == 3).unwrap();
        let inner = cfg.blocks.iter().find(|block| block.start_pc == 5).unwrap();
        assert!(phi_for(&ssa, outer.id, 1).is_some());
        assert!(phi_for(&ssa, inner.id, 2).is_some());
    }

    #[test]
    fn try_catch_reloads_every_register_without_exception_edge_phis() {
        let (cfg, ssa) = build(
            0,
            4,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
                (
                    Op::EnterTry,
                    vec![
                        Operand::Imm32(4),
                        Operand::Imm32(NO_HANDLER_OFFSET),
                        Operand::Register(3),
                    ],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(2)]),
                (
                    Op::LoadGlobalOrThrow,
                    vec![Operand::Register(2), Operand::ConstIndex(0)],
                ),
                (Op::LeaveTry, vec![]),
                (Op::Jump, vec![Operand::Imm32(2)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(1),
                        Operand::Register(0),
                    ],
                ),
                (Op::Nop, vec![]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        let handler = cfg.blocks.iter().find(|block| block.start_pc == 6).unwrap();
        let heads = &ssa.blocks[handler.id.0 as usize].phis;
        assert_eq!(heads.len(), usize::from(ssa.register_count));
        assert!(heads.iter().all(|value| matches!(
            ssa.values[value.0 as usize].def,
            ValueDef::ExceptionInput { block, .. } if block == handler.id
        )));
        assert!(
            !heads
                .iter()
                .any(|value| matches!(ssa.values[value.0 as usize].def, ValueDef::Phi { .. }))
        );

        let add = &ssa.blocks[handler.id.0 as usize].instrs[0];
        for (&input, register) in add.inputs.iter().zip([1_u16, 0]) {
            assert!(matches!(
                ssa.values[input.0 as usize].def,
                ValueDef::ExceptionInput {
                    block,
                    register: owner,
                } if block == handler.id && owner == register
            ));
        }
    }

    #[test]
    fn verifier_rejects_corrupt_phi_input_count() {
        let (cfg, mut ssa) = build(
            1,
            2,
            vec![
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(0)],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)]),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(2)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let join = cfg.blocks.iter().find(|block| block.start_pc == 4).unwrap();
        let phi = phi_for(&ssa, join.id, 1).expect("diamond has a phi");
        let ValueDef::Phi { inputs, .. } = &mut ssa.values[phi.0 as usize].def else {
            unreachable!();
        };
        *inputs = vec![inputs[0]].into_boxed_slice();

        assert_eq!(
            ssa.verify(&cfg, &DominatorTree::compute(&cfg)),
            Err(SsaError::PhiInputCountMismatch {
                value: phi,
                expected: 2,
                actual: 1,
            })
        );
    }
}
