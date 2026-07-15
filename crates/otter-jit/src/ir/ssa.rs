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
//! - Renamed instructions retain the source and destination register identities
//!   needed by later frame-state reconstruction.
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
use otter_vm::{CodeBlock, JitCompileSnapshot, JitInstructionMetadata};
use smallvec::SmallVec;

use super::{
    cfg::{BlockId, ControlFlowGraph, Terminator},
    dom::{DomError, DominanceFrontier, DominatorTree},
    inline::{InlineId, InlineTree},
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
    /// The `undefined` a spliced callee returns when its return carries no
    /// value operand. Defined at the callee frame's entry so it dominates every
    /// return in that frame.
    InlineUndefinedReturn {
        /// Callee frame entry defining it.
        block: BlockId,
    },
    /// Register frame reload at an exception-handler entry.
    ExceptionInput {
        /// Handler block performing the reload.
        block: BlockId,
        /// Reloaded bytecode register.
        register: u16,
    },
    /// The value a spliced call produces: a merge of the callee's returned
    /// values, defined at the call's continuation block and bound to the
    /// caller's result register.
    InlineResult {
        /// Continuation block containing the merge.
        block: BlockId,
        /// Caller register the call writes.
        register: u16,
        /// One value per callee-return predecessor, in predecessor order.
        inputs: Box<[ValueId]>,
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
        /// Frame owning the instruction; [`Self::Op::pc`] is canonical in it.
        inline: InlineId,
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
    /// Frame owning this instruction; [`Self::pc`] is canonical within it.
    pub inline: InlineId,
    /// Original canonical bytecode PC.
    pub pc: u32,
    /// Original bytecode opcode.
    pub op: Op,
    /// SSA values for schema-declared read registers, in operand order.
    pub inputs: SmallVec<[ValueId; 4]>,
    /// Source registers corresponding one-for-one with [`Self::inputs`].
    pub input_registers: SmallVec<[u16; 4]>,
    /// SSA result when the instruction writes one register.
    pub result: Option<ValueId>,
    /// Destination register corresponding to [`Self::result`].
    pub result_register: Option<u16>,
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
    /// Unit entry block.
    pub entry: BlockId,
    /// Number of bytecode virtual registers in the root frame.
    pub register_count: u16,
    /// Per-frame register-window shape, indexed by [`InlineId`].
    pub frames: Vec<SsaFrame>,
}

/// Register-window shape of one frame in the compiled unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SsaFrame {
    /// Register-window length of this frame's body.
    pub register_count: u16,
    /// Formal parameter count of this frame's body.
    pub param_count: u16,
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
    /// An instruction does not retain one source register per SSA input.
    InputRegisterCountMismatch {
        /// Instruction with mismatched input metadata.
        pc: u32,
        /// Number of SSA inputs.
        inputs: usize,
        /// Number of retained source registers.
        registers: usize,
    },
    /// An instruction's result and retained destination register disagree.
    ResultRegisterMismatch {
        /// Instruction with mismatched result metadata.
        pc: u32,
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

/// Dense index spaces over a compiled unit's frames.
///
/// A frame's registers and PCs are canonical only within that frame, so every
/// per-register and per-instruction table is keyed by `(frame, index)` flattened
/// into one dense range per frame.
struct UnitLayout {
    register_base: Vec<u32>,
    register_count: Vec<u16>,
    instruction_base: Vec<u32>,
    total_variables: usize,
    total_instructions: usize,
}

impl UnitLayout {
    fn new(tree: &InlineTree) -> Self {
        let mut register_base = Vec::with_capacity(tree.frames.len());
        let mut register_count = Vec::with_capacity(tree.frames.len());
        let mut instruction_base = Vec::with_capacity(tree.frames.len());
        let mut variables = 0u32;
        let mut instructions = 0u32;
        for frame in &tree.frames {
            register_base.push(variables);
            register_count.push(frame.code_block.register_count);
            instruction_base.push(instructions);
            variables += u32::from(frame.code_block.register_count);
            instructions += frame.instructions.len() as u32;
        }
        Self {
            register_base,
            register_count,
            instruction_base,
            total_variables: variables as usize,
            total_instructions: instructions as usize,
        }
    }

    fn register_count(&self, inline: InlineId) -> u16 {
        self.register_count[inline.0 as usize]
    }

    fn variable(&self, inline: InlineId, register: u16) -> usize {
        self.register_base[inline.0 as usize] as usize + usize::from(register)
    }

    fn instruction(&self, inline: InlineId, pc: u32) -> usize {
        self.instruction_base[inline.0 as usize] as usize + pc as usize
    }
}

/// `true` when any block of `frame` returns without a value operand.
fn frame_has_valueless_return(cfg: &ControlFlowGraph, frame: &crate::ir::inline::InlineFrame) -> bool {
    cfg.blocks.iter().any(|block| {
        block.inline == frame.id
            && matches!(block.terminator, Terminator::InlineReturn { .. })
            && block
                .instr_pcs
                .last()
                .is_some_and(|&pc| frame.instructions[pc as usize].op(frame.code_block.as_ref())
                    != Op::ReturnValue)
    })
}

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
    /// Build SSA for one function with nothing spliced into it.
    pub fn build(view: &JitCompileSnapshot, cfg: &ControlFlowGraph) -> Result<Self, SsaError> {
        Self::build_inlined(&InlineTree::trivial(view), cfg)
    }

    /// Build SSA for a whole compiled unit.
    ///
    /// Each frame owns a private register space, so the renamed variable is
    /// `(frame, register)` rather than `register`. A spliced callee's
    /// parameters are not fresh definitions: they alias the caller's argument
    /// values, which is sound because the call block dominates the callee's
    /// entry. The call's result is defined at the continuation as a merge of
    /// the callee's returned values.
    pub fn build_inlined(tree: &InlineTree, cfg: &ControlFlowGraph) -> Result<Self, SsaError> {
        let layout = UnitLayout::new(tree);
        let instruction_count: usize = cfg.blocks.iter().map(|block| block.instr_pcs.len()).sum();
        // The graph prunes unreachable blocks, so it may cover fewer
        // instructions than the bodies declare — never more. Dense per-frame
        // tables stay full-sized; dead slots are simply never read.
        if instruction_count > layout.total_instructions {
            return Err(SsaError::SnapshotInstructionCountMismatch {
                snapshot: layout.total_instructions,
                cfg: instruction_count,
            });
        }

        let mut flows = vec![RegisterFlow::default(); layout.total_instructions];
        let mut def_blocks = vec![BTreeSet::new(); layout.total_variables];
        for (index, frame) in tree.frames.iter().enumerate() {
            let entry = cfg.frame_entries[index];
            for register in 0..frame.code_block.register_count {
                def_blocks[layout.variable(frame.id, register)].insert(entry);
            }
        }

        let handlers = exception_handlers(cfg);
        for (index, is_handler) in handlers.iter().copied().enumerate() {
            if !is_handler {
                continue;
            }
            let block = &cfg.blocks[index];
            for register in 0..layout.register_count(block.inline) {
                def_blocks[layout.variable(block.inline, register)].insert(block.id);
            }
        }

        // A spliced call stops defining its result register: the continuation's
        // merge owns that definition instead.
        let mut spliced_calls = BTreeSet::new();
        let mut continuations = Vec::new();
        for frame in &tree.frames {
            let Some(call_site) = frame.call_site.as_ref() else {
                continue;
            };
            spliced_calls.insert((call_site.parent, call_site.call_pc));
            let Terminator::InlineCall { continuation, .. } = cfg
                .blocks
                .iter()
                .find(|block| {
                    block.inline == call_site.parent
                        && block.instr_pcs.last() == Some(&call_site.call_pc)
                })
                .map(|block| block.terminator)
                .ok_or(SsaError::MissingNormalPredecessor {
                    block: cfg.frame_entries[frame.id.0 as usize],
                    predecessor: cfg.entry,
                })?
            else {
                return Err(SsaError::MissingNormalPredecessor {
                    block: cfg.frame_entries[frame.id.0 as usize],
                    predecessor: cfg.entry,
                });
            };
            def_blocks[layout.variable(call_site.parent, call_site.result_register)]
                .insert(continuation);
            continuations.push((continuation, frame.id, call_site.clone()));
        }

        for block in &cfg.blocks {
            let frame = &tree.frames[block.inline.0 as usize];
            let code_block = frame.code_block.as_ref();
            for &pc in &block.instr_pcs {
                let instruction = &frame.instructions[pc as usize];
                let actual_pc = instruction.instruction_pc(code_block);
                if actual_pc != pc {
                    return Err(SsaError::InstructionPcMismatch {
                        expected: pc,
                        actual: actual_pc,
                    });
                }
                let mut flow = register_flow(
                    code_block,
                    &frame.instructions,
                    pc,
                    code_block.register_count,
                )?;
                if spliced_calls.contains(&(block.inline, pc)) {
                    flow.def = None;
                }
                if let Some(register) = flow.def {
                    def_blocks[layout.variable(block.inline, register)].insert(block.id);
                }
                flows[layout.instruction(block.inline, pc)] = flow;
            }
        }

        let normal_dom = DominatorTree::compute_normal(cfg);
        let frontier = DominanceFrontier::compute(cfg, &normal_dom);
        let mut phi_registers = vec![BTreeSet::new(); cfg.blocks.len()];
        for frame in &tree.frames {
            for register in 0..layout.register_count(frame.id) {
                let variable = layout.variable(frame.id, register);
                let definitions = &def_blocks[variable];
                let mut worklist: VecDeque<_> = definitions.iter().copied().collect();
                while let Some(definition) = worklist.pop_front() {
                    for &target in frontier.frontier(definition) {
                        let target_index = target.0 as usize;
                        // A frame's registers die with its frame: a phi for them
                        // in another frame's block would merge values that no
                        // longer name anything.
                        if cfg.blocks[target_index].inline != frame.id {
                            continue;
                        }
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
        let mut undefined_return = vec![None; tree.frames.len()];
        for &block_id in normal_dom.reverse_postorder() {
            let block_index = block_id.0 as usize;
            let inline = cfg.blocks[block_index].inline;
            let frame = &tree.frames[inline.0 as usize];
            let register_count = layout.register_count(inline);
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
            } else if block_id == cfg.frame_entries[inline.0 as usize] {
                // The frame's `undefined` return constant sorts ahead of every
                // register-keyed head value.
                if inline != InlineId::ROOT && frame_has_valueless_return(cfg, frame) {
                    let id = append_value(
                        &mut values,
                        ValueDef::InlineUndefinedReturn { block: block_id },
                        block_id,
                    )?;
                    undefined_return[inline.0 as usize] = Some(id);
                    blocks[block_index].phis.push(id);
                }
                for register in 0..register_count {
                    // A spliced frame's parameters alias the caller's argument
                    // values at rename time and get no definition of their own.
                    if inline != InlineId::ROOT && register < frame.code_block.param_count {
                        continue;
                    }
                    let def = if inline == InlineId::ROOT
                        && register < frame.code_block.param_count
                    {
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

            if let Some((_, _, call_site)) = continuations
                .iter()
                .find(|(continuation, _, _)| *continuation == block_id)
            {
                let predecessor_count = normal_predecessors(cfg, block_id).len();
                let id = append_value(
                    &mut values,
                    ValueDef::InlineResult {
                        block: block_id,
                        register: call_site.result_register,
                        inputs: vec![ValueId(u32::MAX); predecessor_count].into_boxed_slice(),
                    },
                    block_id,
                )?;
                blocks[block_index].phis.push(id);
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
                let instruction = &frame.instructions[pc as usize];
                let op = instruction.op(frame.code_block.as_ref());
                let flow = &flows[layout.instruction(inline, pc)];
                let result = if flow.def.is_some() {
                    Some(append_value(
                        &mut values,
                        ValueDef::Op {
                            inline,
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
                    inline,
                    pc,
                    op,
                    inputs: SmallVec::new(),
                    input_registers: flow.uses.clone(),
                    result,
                    result_register: flow.def,
                });
            }
        }

        let mut function = Self {
            blocks,
            values,
            entry: cfg.entry,
            register_count: layout.register_count(InlineId::ROOT),
            frames: tree
                .frames
                .iter()
                .map(|frame| SsaFrame {
                    register_count: frame.code_block.register_count,
                    param_count: frame.code_block.param_count,
                })
                .collect(),
        };
        function.rename(tree, cfg, &normal_dom, &flows, &layout, &undefined_return)?;
        let full_dom = DominatorTree::compute(cfg);
        function.verify(cfg, &full_dom)?;
        Ok(function)
    }

    fn rename(
        &mut self,
        tree: &InlineTree,
        cfg: &ControlFlowGraph,
        normal_dom: &DominatorTree,
        flows: &[RegisterFlow],
        layout: &UnitLayout,
        undefined_return: &[Option<ValueId>],
    ) -> Result<(), SsaError> {
        enum Event {
            Enter(BlockId),
            Exit(Vec<usize>),
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
        let mut stacks = vec![Vec::new(); layout.total_variables];

        for root in roots {
            let mut events = vec![Event::Enter(root)];
            while let Some(event) = events.pop() {
                match event {
                    Event::Exit(pushed) => {
                        for variable in pushed.into_iter().rev() {
                            stacks[variable]
                                .pop()
                                .expect("rename pops exactly the values it pushed");
                        }
                    }
                    Event::Enter(block_id) => {
                        let block_index = block_id.0 as usize;
                        let inline = cfg.blocks[block_index].inline;
                        let mut pushed = Vec::new();

                        // Entering a spliced frame binds its parameters to the
                        // caller's argument values. The call block dominates
                        // this entry, so those values are in scope and no copy
                        // is needed.
                        if block_id == cfg.frame_entries[inline.0 as usize]
                            && let Some(call_site) =
                                tree.frames[inline.0 as usize].call_site.as_ref()
                        {
                            for (parameter, &argument) in
                                call_site.argument_registers.iter().enumerate()
                            {
                                let parameter = u16::try_from(parameter).map_err(|_| {
                                    SsaError::RegisterOutOfRange {
                                        pc: None,
                                        register: u16::MAX,
                                        register_count: layout.register_count(inline),
                                    }
                                })?;
                                let value = stacks
                                    [layout.variable(call_site.parent, argument)]
                                .last()
                                .copied()
                                .ok_or(SsaError::MissingReachingDefinition {
                                    block: block_id,
                                    pc: None,
                                    register: argument,
                                })?;
                                let variable = layout.variable(inline, parameter);
                                stacks[variable].push(value);
                                pushed.push(variable);
                            }
                        }

                        for &value in &self.blocks[block_index].phis {
                            let Some(register) = head_register(&self.values[value.0 as usize].def)
                            else {
                                // A block-head constant names no register.
                                continue;
                            };
                            let variable = layout.variable(inline, register);
                            stacks[variable].push(value);
                            pushed.push(variable);
                        }

                        for instruction_index in 0..self.blocks[block_index].instrs.len() {
                            let pc = self.blocks[block_index].instrs[instruction_index].pc;
                            let flow = &flows[layout.instruction(inline, pc)];
                            let mut inputs = SmallVec::<[ValueId; 4]>::new();
                            for &register in &flow.uses {
                                let value = stacks[layout.variable(inline, register)]
                                    .last()
                                    .copied()
                                    .ok_or(SsaError::MissingReachingDefinition {
                                        block: block_id,
                                        pc: Some(pc),
                                        register,
                                    })?;
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
                                let variable = layout.variable(inline, register);
                                stacks[variable].push(value);
                                pushed.push(variable);
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
                            let successor_inline = cfg.blocks[successor.0 as usize].inline;
                            let successor_heads = self.blocks[successor.0 as usize].phis.clone();
                            for phi in successor_heads {
                                let input = match &self.values[phi.0 as usize].def {
                                    ValueDef::Phi { register, .. } => {
                                        let variable = layout.variable(successor_inline, *register);
                                        stacks[variable].last().copied().ok_or(
                                            SsaError::MissingReachingDefinition {
                                                block: successor,
                                                pc: None,
                                                register: *register,
                                            },
                                        )?
                                    }
                                    // The merged value is what this predecessor
                                    // returns, not what the caller's result
                                    // register held before the call.
                                    ValueDef::InlineResult { register, .. } => self
                                        .returned_value(cfg, block_id, undefined_return)
                                        .ok_or(SsaError::MissingReachingDefinition {
                                            block: successor,
                                            pc: None,
                                            register: *register,
                                        })?,
                                    _ => continue,
                                };
                                match &mut self.values[phi.0 as usize].def {
                                    ValueDef::Phi { inputs, .. }
                                    | ValueDef::InlineResult { inputs, .. } => {
                                        inputs[predecessor_index] = input;
                                    }
                                    _ => unreachable!(),
                                }
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

    /// The value a spliced frame's return block hands back to its caller.
    fn returned_value(
        &self,
        cfg: &ControlFlowGraph,
        block: BlockId,
        undefined_return: &[Option<ValueId>],
    ) -> Option<ValueId> {
        if !matches!(
            cfg.blocks[block.0 as usize].terminator,
            Terminator::InlineReturn { .. }
        ) {
            return None;
        }
        let last = self.blocks[block.0 as usize].instrs.last()?;
        if last.op == Op::ReturnValue {
            return last.inputs.first().copied();
        }
        undefined_return[cfg.blocks[block.0 as usize].inline.0 as usize]
    }

    /// Register-window length of one frame in the unit.
    #[must_use]
    pub fn frame_registers(&self, inline: InlineId) -> u16 {
        self.frames[inline.0 as usize].register_count
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
        // Frame entries and handlers seed their own frame's registers, so every
        // per-register table is sized by the frame that owns the block.
        let mut entry_inputs: Vec<Vec<bool>> = self
            .frames
            .iter()
            .map(|frame| vec![false; usize::from(frame.register_count)])
            .collect();
        let mut handler_inputs: Vec<Vec<bool>> = cfg
            .blocks
            .iter()
            .map(|block| vec![false; usize::from(self.frame_registers(block.inline))])
            .collect();

        for (block_index, block) in self.blocks.iter().enumerate() {
            let block_id = block.id;
            let inline = cfg.blocks[block_index].inline;
            let frame_registers = self.frame_registers(inline);
            let frame_entry = cfg.frame_entries[inline.0 as usize];
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
                        mark_entry_input(
                            &mut entry_inputs[inline.0 as usize],
                            *register,
                            frame_registers,
                        )?;
                        (1_u8, *register)
                    }
                    // Every frame seeds the registers its call ABI does not
                    // fill, not just the unit's outermost entry.
                    ValueDef::Uninitialized { register } if block_id == frame_entry => {
                        mark_entry_input(
                            &mut entry_inputs[inline.0 as usize],
                            *register,
                            frame_registers,
                        )?;
                        (1, *register)
                    }
                    ValueDef::InlineUndefinedReturn { block: owner }
                        if *owner == block_id
                            && block_id == frame_entry
                            && inline != InlineId::ROOT =>
                    {
                        // A frame-entry constant that no register names; it
                        // sorts ahead of every register-keyed head value.
                        (0, 0)
                    }
                    ValueDef::ExceptionInput {
                        block: owner,
                        register,
                    } if *owner == block_id && handlers[block_index] => {
                        mark_handler_input(
                            &mut handler_inputs[block_index],
                            block_id,
                            *register,
                            frame_registers,
                        )?;
                        (1, *register)
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
                        (2, *register)
                    }
                    // A spliced call's result merges the callee's returned
                    // values; its predecessors are the callee's return blocks.
                    ValueDef::InlineResult {
                        block: owner,
                        register,
                        inputs,
                    } if *owner == block_id => {
                        if inputs.len() != normal_predecessors.len() {
                            return Err(SsaError::PhiInputCountMismatch {
                                value: value_id,
                                expected: normal_predecessors.len(),
                                actual: inputs.len(),
                            });
                        }
                        if !normal_predecessors.iter().all(|&predecessor| {
                            matches!(
                                cfg.blocks[predecessor.0 as usize].terminator,
                                Terminator::InlineReturn { .. }
                            )
                        }) {
                            return Err(SsaError::InvalidHeadDefinition {
                                value: value_id,
                                block: block_id,
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
                check_register(None, register, frame_registers)?;
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
                if instruction.input_registers.len() != instruction.inputs.len() {
                    return Err(SsaError::InputRegisterCountMismatch {
                        pc: instruction.pc,
                        inputs: instruction.inputs.len(),
                        registers: instruction.input_registers.len(),
                    });
                }
                for &register in &instruction.input_registers {
                    check_register(Some(instruction.pc), register, self.register_count)?;
                }
                if instruction.result.is_some() != instruction.result_register.is_some() {
                    return Err(SsaError::ResultRegisterMismatch { pc: instruction.pc });
                }
                if let Some(register) = instruction.result_register {
                    check_register(Some(instruction.pc), register, self.register_count)?;
                }
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
                        ValueDef::Op {
                            inline,
                            pc,
                            op,
                            inputs,
                        } if *inline == instruction.inline
                            && *pc == instruction.pc
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
        // Every frame seeds each of its registers at its entry, except the
        // parameters of a spliced frame, which alias the caller's arguments.
        for (index, frame) in self.frames.iter().enumerate() {
            let inline = InlineId(index as u32);
            let entry = cfg.frame_entries[index];
            if handlers[entry.0 as usize] {
                continue;
            }
            let aliased_parameters = if inline == InlineId::ROOT {
                0
            } else {
                frame.param_count
            };
            for register in aliased_parameters..frame.register_count {
                if !entry_inputs[index][usize::from(register)] {
                    return Err(SsaError::MissingEntryInput { register });
                }
            }
        }
        for (block_index, is_handler) in handlers.iter().copied().enumerate() {
            if is_handler {
                let inline = cfg.blocks[block_index].inline;
                for register in 0..self.frame_registers(inline) {
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
    code_block: &CodeBlock,
    instructions: &[JitInstructionMetadata],
    pc: u32,
    register_count: u16,
) -> Result<RegisterFlow, SsaError> {
    let instruction = &instructions[pc as usize];
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
        | ValueDef::InlineResult { register, .. }
        | ValueDef::Phi { register, .. } => Some(*register),
        // A spliced callee's `undefined` return value is a block-head constant
        // that no register names.
        ValueDef::InlineUndefinedReturn { .. } | ValueDef::Op { .. } => None,
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

    /// Splice a one-parameter callee into `r0 = r1(r2); return r0`.
    fn spliced(callee_body: Vec<(Op, Vec<Operand>)>) -> (ControlFlowGraph, SsaFunction, InlineTree) {
        let mut view = snapshot(
            1,
            8,
            vec![
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
            ],
        );
        let callee_instructions: Vec<JitTestInstruction> = callee_body
            .into_iter()
            .enumerate()
            .map(|(pc, (op, operands))| {
                JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands)
            })
            .collect();
        let callee_view = JitCompileSnapshot::without_feedback(9, 1, 4, callee_instructions);
        let call_byte_pc = view.instructions[0].byte_pc;
        view.inline_callees.insert(
            call_byte_pc,
            otter_vm::JitInlineCallee {
                code_block: std::sync::Arc::clone(&callee_view.code_block),
                function_id: 9,
                param_count: 1,
                register_count: callee_view.code_block.register_count,
                instructions: callee_view.instructions,
            },
        );
        let tree = InlineTree::build(&view);
        assert_eq!(tree.frames.len(), 2, "the fixture must splice");
        let cfg = ControlFlowGraph::build_inlined(&tree).expect("a spliced CFG builds");
        let ssa = SsaFunction::build_inlined(&tree, &cfg).expect("a spliced SSA builds");
        ssa.verify(&cfg, &DominatorTree::compute(&cfg))
            .expect("a spliced SSA verifies");
        (cfg, ssa, tree)
    }

    #[test]
    fn a_spliced_parameter_aliases_the_caller_argument_value() {
        // The callee body is `return r0`.
        let (cfg, ssa, _) = spliced(vec![(Op::ReturnValue, vec![Operand::Register(0)])]);

        // The caller's argument register r2 is an entry seed of the root frame.
        let argument = ssa.blocks[0]
            .phis
            .iter()
            .copied()
            .find(|&value| {
                matches!(ssa.values[value.0 as usize].def, ValueDef::Uninitialized { register } if register == 2)
            })
            .expect("r2 is seeded at the root entry");

        // The callee's `return r0` reads exactly that value: no copy, no fresh
        // parameter definition.
        let callee_entry = cfg.frame_entries[1];
        let callee_block = &ssa.blocks[callee_entry.0 as usize];
        assert!(
            callee_block.phis.iter().all(|&value| !matches!(
                ssa.values[value.0 as usize].def,
                ValueDef::Param { .. }
            )),
            "a spliced frame defines no parameters of its own",
        );
        let ret = callee_block.instrs.last().expect("the callee returns");
        assert_eq!(ret.op, Op::ReturnValue);
        assert_eq!(ret.inline, InlineId(1));
        assert_eq!(ret.inputs.as_slice(), [argument]);
    }

    #[test]
    fn the_call_result_merges_the_callee_returned_value() {
        let (cfg, ssa, _) = spliced(vec![(Op::ReturnValue, vec![Operand::Register(0)])]);

        // The spliced call defines nothing; the continuation's merge does.
        let call = ssa.blocks[0].instrs.last().expect("the call is emitted");
        assert_eq!(call.op, Op::Call);
        assert_eq!(call.result, None);
        assert_eq!(call.result_register, None);

        let Terminator::InlineCall { continuation, .. } = cfg.blocks[0].terminator else {
            panic!("the call block ends in a splice");
        };
        let merge = ssa.blocks[continuation.0 as usize]
            .phis
            .iter()
            .copied()
            .find(|&value| matches!(ssa.values[value.0 as usize].def, ValueDef::InlineResult { .. }))
            .expect("the continuation merges the call result");
        let ValueDef::InlineResult {
            register, inputs, ..
        } = &ssa.values[merge.0 as usize].def
        else {
            unreachable!()
        };
        assert_eq!(*register, 0, "the merge binds the caller's result register");

        // Its only input is what the callee returned.
        let callee_return = &ssa.blocks[cfg.frame_entries[1].0 as usize];
        let returned = callee_return.instrs.last().expect("the callee returns").inputs[0];
        assert_eq!(inputs.as_ref(), [returned]);

        // The caller's own `return r0` then reads the merge, not its stale r0.
        let caller_return = ssa.blocks[continuation.0 as usize]
            .instrs
            .last()
            .expect("the caller returns");
        assert_eq!(caller_return.inputs.as_slice(), [merge]);
    }

    #[test]
    fn a_valueless_callee_return_merges_undefined() {
        // The callee body is a bare `return`.
        let (cfg, ssa, _) = spliced(vec![(Op::ReturnUndefined, Vec::new())]);

        let undefined = ssa.blocks[cfg.frame_entries[1].0 as usize]
            .phis
            .iter()
            .copied()
            .find(|&value| {
                matches!(
                    ssa.values[value.0 as usize].def,
                    ValueDef::InlineUndefinedReturn { .. }
                )
            })
            .expect("a valueless return needs an undefined to merge");

        let Terminator::InlineCall { continuation, .. } = cfg.blocks[0].terminator else {
            panic!("the call block ends in a splice");
        };
        let merge = ssa.blocks[continuation.0 as usize]
            .phis
            .iter()
            .copied()
            .find(|&value| matches!(ssa.values[value.0 as usize].def, ValueDef::InlineResult { .. }))
            .expect("the continuation merges the call result");
        let ValueDef::InlineResult { inputs, .. } = &ssa.values[merge.0 as usize].def else {
            unreachable!()
        };
        assert_eq!(inputs.as_ref(), [undefined]);
    }

    #[test]
    fn each_frame_keeps_a_private_register_space() {
        // Both frames use register 0 for different things: the caller's result
        // and the callee's parameter. They must never be the same variable.
        let (cfg, ssa, _) = spliced(vec![(Op::ReturnValue, vec![Operand::Register(0)])]);

        assert_eq!(ssa.frames.len(), 2);
        assert_eq!(ssa.frame_registers(InlineId::ROOT), 8);
        assert_eq!(ssa.frame_registers(InlineId(1)), 4);

        let callee_r0 = ssa.blocks[cfg.frame_entries[1].0 as usize]
            .instrs
            .last()
            .expect("the callee returns")
            .inputs[0];
        let root_r0 = ssa.blocks[0]
            .phis
            .iter()
            .copied()
            .find(|&value| {
                matches!(ssa.values[value.0 as usize].def, ValueDef::Param { register: 0, .. })
            })
            .expect("the root's r0 is its parameter");
        assert_ne!(
            callee_r0, root_r0,
            "the callee's r0 is the caller's argument, not the caller's r0",
        );
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
