//! Bytecode analysis the graph builder walks: decoded instructions, basic
//! blocks, loops, and interpreter-register liveness.
//!
//! # Contents
//! - [`Instruction`] — one decoded instruction: opcode, logical and byte PC,
//!   the registers it reads and writes, and its normal successors.
//! - [`RegisterSet`] — a dense bitset over interpreter registers.
//! - [`Analysis`] — the whole-function result: blocks in reverse post-order,
//!   predecessor counts, loop headers with their assigned registers, and the
//!   live-in register set before every instruction.
//!
//! # Invariants
//! - Register roles come only from the opcode schema (`operand_spec_at`);
//!   the one implicit read (`CallForwardArguments` over register-held mapped
//!   formals) is added here, so liveness and frame states agree with the
//!   interpreter.
//! - A block ends at a control instruction or before a block start. Every
//!   jump target and every instruction after a branch, jump, return or throw
//!   starts a block.
//! - A backward edge must target a block that dominates its source; any other
//!   cycle makes the function irreducible, which the builder handles by
//!   deoptimizing at the irreducible entry instead of modelling the cycle.
//! - Every instruction inside an exception region keeps the registers its
//!   handler reads live: a throw materializes the frame at the throwing PC and
//!   the interpreter enters the handler from there.
//!
//! # See also
//! - [`super::builder`] — the abstract interpreter consuming this analysis.
//! - `otter_bytecode::opcode_schema` — the operand-role authority.

use otter_bytecode::opcode_schema::{
    OperandKind, RegisterAccess, RegisterSource, opcode_schema, operand_spec_at,
};
use otter_bytecode::{Op, Operand};
use otter_vm::JitCompileSnapshot;
use smallvec::SmallVec;

/// Dense set of interpreter registers.
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct RegisterSet {
    words: Box<[u64]>,
}

impl std::fmt::Debug for RegisterSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl RegisterSet {
    pub(crate) fn new(width: usize) -> Self {
        Self {
            words: vec![0; width.div_ceil(64)].into_boxed_slice(),
        }
    }

    pub(crate) fn contains(&self, register: u16) -> bool {
        let index = usize::from(register);
        self.words
            .get(index / 64)
            .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    pub(crate) fn insert(&mut self, register: u16) {
        let index = usize::from(register);
        if let Some(word) = self.words.get_mut(index / 64) {
            *word |= 1 << (index % 64);
        }
    }

    pub(crate) fn remove(&mut self, register: u16) {
        let index = usize::from(register);
        if let Some(word) = self.words.get_mut(index / 64) {
            *word &= !(1 << (index % 64));
        }
    }

    /// Add every member of `other`; report whether this set grew.
    pub(crate) fn union_with(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (word, &incoming) in self.words.iter_mut().zip(other.words.iter()) {
            let merged = *word | incoming;
            changed |= merged != *word;
            *word = merged;
        }
        changed
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.words.iter().enumerate().flat_map(|(index, &word)| {
            let mut bits = word;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let bit = bits.trailing_zeros();
                bits &= bits - 1;
                Some((index * 64 + bit as usize) as u16)
            })
        })
    }
}

/// Normal control transfer at the end of an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Continue at the next instruction.
    Next,
    /// Continue only at `target`.
    Jump { target: u32 },
    /// Continue at `target` or the next instruction.
    Branch { target: u32 },
    /// Leave the frame (return, throw, or suspension).
    Exit,
}

/// One decoded instruction.
#[derive(Debug, Clone)]
pub(crate) struct Instruction {
    pub(crate) op: Op,
    pub(crate) pc: u32,
    pub(crate) byte_pc: u32,
    pub(crate) operands: SmallVec<[Operand; 4]>,
    pub(crate) reads: SmallVec<[u16; 4]>,
    pub(crate) writes: SmallVec<[u16; 2]>,
    pub(crate) flow: Flow,
}

impl Instruction {
    pub(crate) fn register(&self, index: usize) -> Option<u16> {
        match self.operands.get(index)? {
            Operand::Register(register) => Some(*register),
            _ => None,
        }
    }

    pub(crate) fn imm32(&self, index: usize) -> Option<i32> {
        match self.operands.get(index)? {
            Operand::Imm32(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn const_index(&self, index: usize) -> Option<u32> {
        match self.operands.get(index)? {
            Operand::ConstIndex(value) => Some(*value),
            _ => None,
        }
    }
}

/// One basic block of bytecode.
#[derive(Debug, Clone)]
pub(crate) struct Block {
    /// First logical PC.
    pub(crate) start: u32,
    /// One past the last logical PC.
    pub(crate) end: u32,
    /// Successor block indices in normal-flow order (branch target first).
    pub(crate) successors: SmallVec<[usize; 2]>,
    /// Forward predecessors (edges from blocks earlier in reverse
    /// post-order).
    pub(crate) forward_predecessors: u32,
    /// Backward predecessors; non-zero only for loop headers.
    pub(crate) back_predecessors: u32,
    /// Whether any path from entry reaches this block.
    pub(crate) reachable: bool,
}

/// Loop facts for one header block.
#[derive(Debug, Clone)]
pub(crate) struct LoopInfo {
    /// Registers written anywhere inside the loop body.
    pub(crate) assigned: RegisterSet,
}

/// Whole-function analysis.
#[derive(Debug)]
pub(crate) struct Analysis {
    pub(crate) instructions: Vec<Instruction>,
    pub(crate) blocks: Vec<Block>,
    /// Block index of every instruction.
    pub(crate) block_of: Vec<usize>,
    /// Reachable blocks in reverse post-order; the builder visits this order.
    pub(crate) order: Vec<usize>,
    /// Loop facts keyed by header block.
    pub(crate) loops: rustc_hash::FxHashMap<usize, LoopInfo>,
    /// Headers reached through an edge their dominator relation does not
    /// admit; such a header is entered only through a deopt.
    pub(crate) irreducible: rustc_hash::FxHashSet<usize>,
    /// Registers live before each instruction.
    pub(crate) live_in: Vec<RegisterSet>,
    pub(crate) register_count: u16,
}

/// Why a bytecode body cannot be analysed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnalysisError {
    /// An operand disagrees with its schema row.
    MalformedOperand { pc: u32 },
    /// A relative target leaves the function.
    TargetOutOfRange { pc: u32 },
    /// The function has no instructions.
    Empty,
}

impl Analysis {
    pub(crate) fn build(view: &JitCompileSnapshot) -> Result<Self, AnalysisError> {
        let code = view.code_block.as_ref();
        let count = view.instructions.len();
        if count == 0 {
            return Err(AnalysisError::Empty);
        }
        let register_count = code.register_count;
        let mut instructions = Vec::with_capacity(count);
        for (index, metadata) in view.instructions.iter().enumerate() {
            let pc = index as u32;
            let op = metadata.op(code);
            let view_operands = metadata.operand_view(code);
            let operands: SmallVec<[Operand; 4]> = view_operands.iter().collect();
            let mut reads = SmallVec::new();
            let mut writes = SmallVec::new();
            for (position, operand) in operands.iter().enumerate() {
                let spec = operand_spec_at(op, position)
                    .ok_or(AnalysisError::MalformedOperand { pc })?;
                if OperandKind::of(operand) != spec.kind {
                    return Err(AnalysisError::MalformedOperand { pc });
                }
                if spec.register_access == RegisterAccess::None {
                    continue;
                }
                let register = match (spec.register_source, *operand) {
                    (Some(RegisterSource::RegisterOperand), Operand::Register(register)) => {
                        register
                    }
                    (Some(RegisterSource::Imm32RegisterIndex), Operand::Imm32(register)) => {
                        u16::try_from(register)
                            .map_err(|_| AnalysisError::MalformedOperand { pc })?
                    }
                    _ => return Err(AnalysisError::MalformedOperand { pc }),
                };
                if register >= register_count {
                    return Err(AnalysisError::MalformedOperand { pc });
                }
                match spec.register_access {
                    RegisterAccess::Read => reads.push(register),
                    RegisterAccess::Write => writes.push(register),
                    RegisterAccess::None => {}
                }
            }
            if op == Op::CallForwardArguments {
                for (_, storage) in code.forwarded_argument_bindings() {
                    if let otter_bytecode::ArgumentBindingStorage::Register { reg } = storage
                        && !reads.contains(&reg)
                    {
                        reads.push(reg);
                    }
                }
            }
            let relative = |operand: usize| -> Result<u32, AnalysisError> {
                let delta = match operands.get(operand) {
                    Some(Operand::Imm32(delta)) => i64::from(*delta),
                    _ => return Err(AnalysisError::MalformedOperand { pc }),
                };
                let target = i64::from(pc) + 1 + delta;
                u32::try_from(target)
                    .ok()
                    .filter(|&target| (target as usize) < count)
                    .ok_or(AnalysisError::TargetOutOfRange { pc })
            };
            let flow = match op {
                Op::Jump => Flow::Jump {
                    target: relative(0)?,
                },
                Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => Flow::Branch {
                    target: relative(0)?,
                },
                _ => match opcode_schema(op).control_flow {
                    otter_bytecode::opcode_schema::ControlFlow::Return
                    | otter_bytecode::opcode_schema::ControlFlow::Throw => Flow::Exit,
                    _ => Flow::Next,
                },
            };
            instructions.push(Instruction {
                op,
                pc,
                byte_pc: metadata.byte_pc,
                operands,
                reads,
                writes,
                flow,
            });
        }

        // Block boundaries: the CodeBlock's starts, every target, and every
        // instruction after a control transfer.
        let mut is_start = vec![false; count];
        is_start[0] = true;
        for &start in code.block_starts() {
            if let Some(slot) = is_start.get_mut(start as usize) {
                *slot = true;
            }
        }
        for instruction in &instructions {
            match instruction.flow {
                Flow::Jump { target } | Flow::Branch { target } => {
                    is_start[target as usize] = true;
                }
                _ => {}
            }
            if instruction.flow != Flow::Next
                && let Some(next) = is_start.get_mut(instruction.pc as usize + 1)
            {
                *next = true;
            }
        }
        // Handlers are entered only through a throw, but their targets still
        // begin blocks so a normal edge into them stays well formed.
        for handler in code.control_flow().handlers() {
            if let Some(slot) = is_start.get_mut(handler.target as usize) {
                *slot = true;
            }
        }
        let mut blocks = Vec::new();
        let mut block_of = vec![0usize; count];
        let mut start = 0u32;
        for pc in 1..=count {
            if pc == count || is_start[pc] {
                let index = blocks.len();
                for slot in &mut block_of[start as usize..pc] {
                    *slot = index;
                }
                blocks.push(Block {
                    start,
                    end: pc as u32,
                    successors: SmallVec::new(),
                    forward_predecessors: 0,
                    back_predecessors: 0,
                    reachable: false,
                });
                start = pc as u32;
            }
        }
        for index in 0..blocks.len() {
            let last = &instructions[blocks[index].end as usize - 1];
            let next_block = (blocks[index].end as usize) < count;
            let successors: SmallVec<[usize; 2]> = match last.flow {
                Flow::Next if next_block => smallvec::smallvec![index + 1],
                Flow::Next | Flow::Exit => SmallVec::new(),
                Flow::Jump { target } => smallvec::smallvec![block_of[target as usize]],
                Flow::Branch { target } => {
                    let mut successors = smallvec::smallvec![block_of[target as usize]];
                    if next_block {
                        successors.push(index + 1);
                    }
                    successors
                }
            };
            blocks[index].successors = successors;
        }

        // Reverse post-order from the entry block.
        let mut order = Vec::with_capacity(blocks.len());
        let mut visited = vec![false; blocks.len()];
        let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
        visited[0] = true;
        while let Some(&mut (block, ref mut next)) = stack.last_mut() {
            if let Some(&successor) = blocks[block].successors.get(*next) {
                *next += 1;
                if !visited[successor] {
                    visited[successor] = true;
                    stack.push((successor, 0));
                }
            } else {
                order.push(block);
                stack.pop();
            }
        }
        order.reverse();
        let mut rank = vec![usize::MAX; blocks.len()];
        for (position, &block) in order.iter().enumerate() {
            rank[block] = position;
            blocks[block].reachable = true;
        }

        // Dominators (iterative over RPO) decide which back edges form
        // natural loops.
        let mut idom = vec![usize::MAX; blocks.len()];
        idom[0] = 0;
        let mut predecessors: Vec<SmallVec<[usize; 2]>> = vec![SmallVec::new(); blocks.len()];
        for &block in &order {
            for &successor in &blocks[block].successors {
                predecessors[successor].push(block);
            }
        }
        let intersect = |idom: &[usize], mut a: usize, mut b: usize| {
            while a != b {
                while rank[a] > rank[b] {
                    a = idom[a];
                }
                while rank[b] > rank[a] {
                    b = idom[b];
                }
            }
            a
        };
        loop {
            let mut changed = false;
            for &block in order.iter().skip(1) {
                let mut new_idom = usize::MAX;
                for &predecessor in &predecessors[block] {
                    if idom[predecessor] == usize::MAX {
                        continue;
                    }
                    new_idom = if new_idom == usize::MAX {
                        predecessor
                    } else {
                        intersect(&idom, predecessor, new_idom)
                    };
                }
                if new_idom != idom[block] {
                    idom[block] = new_idom;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let dominates = |a: usize, mut b: usize| loop {
            if a == b {
                return true;
            }
            if b == 0 {
                return false;
            }
            b = idom[b];
        };

        let mut loops = rustc_hash::FxHashMap::default();
        let mut irreducible = rustc_hash::FxHashSet::default();
        for &block in &order {
            for &successor in &blocks[block].successors.clone() {
                if rank[successor] > rank[block] {
                    blocks[successor].forward_predecessors += 1;
                } else if dominates(successor, block) {
                    blocks[successor].back_predecessors += 1;
                    loops.entry(successor).or_insert_with(|| LoopInfo {
                        assigned: RegisterSet::new(usize::from(register_count)),
                    });
                } else {
                    irreducible.insert(successor);
                }
            }
        }
        // Loop bodies: every block that reaches a back edge without leaving
        // through the header.
        for (&header, info) in &mut loops {
            let mut body = vec![false; blocks.len()];
            body[header] = true;
            let mut pending: Vec<usize> = predecessors[header]
                .iter()
                .copied()
                .filter(|&predecessor| dominates(header, predecessor))
                .collect();
            while let Some(block) = pending.pop() {
                if body[block] {
                    continue;
                }
                body[block] = true;
                pending.extend(predecessors[block].iter().copied());
            }
            for (block, &inside) in body.iter().enumerate() {
                if !inside {
                    continue;
                }
                for pc in blocks[block].start..blocks[block].end {
                    for &register in &instructions[pc as usize].writes {
                        info.assigned.insert(register);
                    }
                }
            }
        }

        let live_in = liveness(view, &instructions, &blocks, &block_of, register_count);
        Ok(Self {
            instructions,
            blocks,
            block_of,
            order,
            loops,
            irreducible,
            live_in,
            register_count,
        })
    }

    /// Registers live immediately before the instruction at `pc` runs.
    pub(crate) fn live_at(&self, pc: u32) -> &RegisterSet {
        &self.live_in[pc as usize]
    }
}

/// Backward liveness to a fixpoint over every block, handler bodies included:
/// a handler reachable only by a throw still decides what its protected
/// instructions keep live.
fn liveness(
    view: &JitCompileSnapshot,
    instructions: &[Instruction],
    blocks: &[Block],
    block_of: &[usize],
    register_count: u16,
) -> Vec<RegisterSet> {
    let width = usize::from(register_count);
    let control_flow = view.code_block.control_flow();
    // The handler a throw at each protected instruction lands in.
    let handler_of = |pc: u32| -> Option<(u32, u16)> {
        let handler = control_flow.handler_at(pc)?;
        Some((handler.target, handler.exception))
    };
    let mut block_live_in = vec![RegisterSet::new(width); blocks.len()];
    let mut live_in = vec![RegisterSet::new(width); instructions.len()];
    loop {
        let mut changed = false;
        for block in (0..blocks.len()).rev() {
            let mut live = RegisterSet::new(width);
            for &successor in &blocks[block].successors {
                live.union_with(&block_live_in[successor]);
            }
            for pc in (blocks[block].start..blocks[block].end).rev() {
                let instruction = &instructions[pc as usize];
                for &register in &instruction.writes {
                    live.remove(register);
                }
                for &register in &instruction.reads {
                    live.insert(register);
                }
                if let Some((handler, exception)) = handler_of(pc) {
                    let mut handler_live = block_live_in[block_of[handler as usize]].clone();
                    handler_live.remove(exception);
                    live.union_with(&handler_live);
                }
                live_in[pc as usize] = live.clone();
            }
            if live != block_live_in[block] {
                block_live_in[block] = live;
                changed = true;
            }
        }
        if !changed {
            return live_in;
        }
    }
}
