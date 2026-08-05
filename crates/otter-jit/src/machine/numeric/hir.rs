//! Typed numeric HIR and control-flow construction.
//!
//! # Contents
//! - [`NumericFunction`] — bounded numeric SSA graph with explicit blocks.
//! - [`NumericBlock`] and [`NumericTerminator`] — predecessor/successor edges,
//!   block parameters, edge arguments, branches, and returns.
//! - [`NumericNode`] — typed parameters, constants, arithmetic, and comparison.
//!
//! # Invariants
//! - Every accepted parameter is guarded as a JavaScript Number before effects.
//! - Accepted nodes cannot allocate, call, touch the heap, or throw.
//! - Register merges become typed block parameters; no interpreter slot reaches
//!   Machine IR.
//! - Loop headers receive explicit parameters for every numeric value live from
//!   a forward predecessor; backedge arguments are attached after all blocks
//!   are lowered.
//! - HIR preserves source CFG edges; selection splits critical edges before
//!   allocator move placement.

use std::collections::BTreeMap;

use otter_bytecode::{Op, Operand};
use otter_vm::{JitCompileSnapshot, JitInstructionMetadata};

const MAX_FUNCTION_INSTRUCTIONS: usize = 512;
const MAX_FUNCTION_PARAMETERS: u16 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct NumericValue(pub(super) usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericType {
    Int32,
    Number,
    Boolean,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum NumericNode {
    Parameter(u16),
    BlockParameter(NumericType),
    IntegerConstant(i32),
    Constant(f64),
    WidenInt32(NumericValue),
    IntegerAdd(NumericValue, NumericValue),
    IntegerSub(NumericValue, NumericValue),
    IntegerAddImmediate(NumericValue, i32),
    IntegerSubImmediate(NumericValue, i32),
    IntegerAnd(NumericValue, NumericValue),
    IntegerOr(NumericValue, NumericValue),
    IntegerXor(NumericValue, NumericValue),
    IntegerShiftLeft(NumericValue, NumericValue),
    IntegerShiftRight(NumericValue, NumericValue),
    IntegerNot(NumericValue),
    IntegerAndImmediate(NumericValue, i32),
    IntegerLessThanImmediate(NumericValue, i32),
    IntegerEqualImmediate(NumericValue, i32),
    IntegerNotEqualImmediate(NumericValue, i32),
    Add(NumericValue, NumericValue),
    Sub(NumericValue, NumericValue),
    Mul(NumericValue, NumericValue),
    Div(NumericValue, NumericValue),
    Neg(NumericValue),
    LessThan(NumericValue, NumericValue),
}

impl NumericNode {
    pub(super) const fn value_type(self) -> NumericType {
        match self {
            Self::IntegerConstant(..)
            | Self::IntegerAdd(..)
            | Self::IntegerSub(..)
            | Self::IntegerAddImmediate(..)
            | Self::IntegerSubImmediate(..)
            | Self::IntegerAnd(..)
            | Self::IntegerOr(..)
            | Self::IntegerXor(..)
            | Self::IntegerShiftLeft(..)
            | Self::IntegerShiftRight(..)
            | Self::IntegerNot(..)
            | Self::IntegerAndImmediate(..)
            | Self::BlockParameter(NumericType::Int32) => NumericType::Int32,
            Self::LessThan(..)
            | Self::IntegerLessThanImmediate(..)
            | Self::IntegerEqualImmediate(..)
            | Self::IntegerNotEqualImmediate(..)
            | Self::BlockParameter(NumericType::Boolean) => NumericType::Boolean,
            Self::Parameter(..)
            | Self::BlockParameter(NumericType::Number)
            | Self::Constant(..)
            | Self::WidenInt32(..)
            | Self::Add(..)
            | Self::Sub(..)
            | Self::Mul(..)
            | Self::Div(..)
            | Self::Neg(..) => NumericType::Number,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisterState {
    Unset,
    Undefined,
    Value(NumericValue),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericTerminator {
    Jump,
    Branch {
        condition: NumericValue,
        when_true: bool,
    },
    Return(NumericValue),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NumericBlock {
    pub(super) predecessors: Vec<usize>,
    pub(super) successors: Vec<usize>,
    pub(super) parameters: Vec<NumericValue>,
    pub(super) successor_arguments: Vec<Vec<NumericValue>>,
    pub(super) nodes: Vec<NumericValue>,
    pub(super) terminator: NumericTerminator,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NumericFunction {
    pub(super) nodes: Vec<NumericNode>,
    pub(super) blocks: Vec<NumericBlock>,
    pub(super) frame_states: Vec<NumericFrameState>,
    pub(super) parameter_count: u16,
    pub(super) register_count: u16,
    pub(super) arithmetic_op_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericFrameSlot {
    Value(NumericValue),
    Undefined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NumericFrameState {
    pub(super) point: NumericFramePoint,
    pub(super) function_id: u32,
    pub(super) byte_pc: u32,
    pub(super) slots: Vec<NumericFrameSlot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum NumericFramePoint {
    Node(NumericValue),
    Backedge { predecessor: usize, edge: usize },
}

#[derive(Debug, Clone, Copy)]
enum RawTerminator {
    Jump,
    Branch { when_true: bool },
    Return,
}

#[derive(Debug, Clone)]
struct RawBlock {
    start: usize,
    end: usize,
    predecessors: Vec<usize>,
    successors: Vec<usize>,
    terminator: RawTerminator,
}

impl NumericFunction {
    pub(super) fn build(view: &JitCompileSnapshot) -> Option<Self> {
        let code = view.code_block.as_ref();
        let parameter_count = code.param_count;
        let register_count = code.register_count;
        if code.is_async
            || code.is_generator
            || code.is_async_generator
            || parameter_count > register_count
            || parameter_count > MAX_FUNCTION_PARAMETERS
            || view.instructions.is_empty()
            || view.instructions.len() > MAX_FUNCTION_INSTRUCTIONS
            || !code.control_flow().exception_regions().is_empty()
        {
            return None;
        }

        let raw_blocks = build_raw_blocks(view)?;
        let live_in = build_liveness(view, &raw_blocks, register_count)?;
        let instruction_live_in =
            build_instruction_liveness(view, &raw_blocks, &live_in, register_count)?;
        let mut nodes = Vec::with_capacity(view.instructions.len() + register_count as usize);
        let mut entry = vec![RegisterState::Unset; usize::from(register_count)];
        let mut entry_nodes = Vec::with_capacity(parameter_count as usize);
        for parameter in 0..parameter_count {
            let value = push(&mut nodes, NumericNode::Parameter(parameter));
            entry[usize::from(parameter)] = RegisterState::Value(value);
            entry_nodes.push(value);
        }

        let mut blocks = Vec::with_capacity(raw_blocks.len());
        let mut out_states = Vec::<Vec<RegisterState>>::with_capacity(raw_blocks.len());
        let mut parameter_registers = Vec::<Vec<u16>>::with_capacity(raw_blocks.len());
        let mut arithmetic_op_count = 0usize;
        let mut frame_states = Vec::new();

        for (block_index, raw) in raw_blocks.iter().enumerate() {
            let (mut registers, mut parameters, mut parameter_regs) = if block_index == 0 {
                (entry.clone(), Vec::new(), Vec::new())
            } else if raw
                .predecessors
                .iter()
                .any(|&predecessor| predecessor >= block_index)
            {
                let forward_predecessors = raw
                    .predecessors
                    .iter()
                    .copied()
                    .filter(|&predecessor| predecessor < block_index)
                    .collect::<Vec<_>>();
                merge_predecessors(
                    &forward_predecessors,
                    &live_in[block_index],
                    &out_states,
                    &mut nodes,
                )?
            } else {
                merge_predecessors(
                    &raw.predecessors,
                    &live_in[block_index],
                    &out_states,
                    &mut nodes,
                )?
            };
            if raw
                .predecessors
                .iter()
                .any(|&predecessor| predecessor >= block_index)
            {
                force_loop_parameters(
                    &mut registers,
                    &mut parameters,
                    &mut parameter_regs,
                    &mut nodes,
                    &live_in[block_index],
                )?;
            }
            let mut block_nodes = if block_index == 0 {
                entry_nodes.clone()
            } else {
                Vec::new()
            };
            block_nodes.extend(parameters.iter().copied());
            let terminal_pc = raw.end.checked_sub(1)?;
            for (pc, instruction_live) in instruction_live_in
                .iter()
                .enumerate()
                .take(raw.end)
                .skip(raw.start)
            {
                let instruction = &view.instructions[pc];
                let op = instruction.op(code);
                if pc == terminal_pc
                    && matches!(
                        op,
                        Op::Jump | Op::JumpIfTrue | Op::JumpIfFalse | Op::ReturnValue
                    )
                {
                    break;
                }
                lower_instruction(
                    instruction,
                    code,
                    &mut registers,
                    &mut nodes,
                    &mut block_nodes,
                    &mut arithmetic_op_count,
                    instruction_live,
                    &mut frame_states,
                    code.id,
                )?;
            }

            let terminal = &view.instructions[terminal_pc];
            let terminator = match raw.terminator {
                RawTerminator::Jump => NumericTerminator::Jump,
                RawTerminator::Branch { when_true } => {
                    let condition = read_boolean(&registers, &nodes, register(terminal, code, 1)?)?;
                    NumericTerminator::Branch {
                        condition,
                        when_true,
                    }
                }
                RawTerminator::Return => NumericTerminator::Return(read_number(
                    &registers,
                    &nodes,
                    register(terminal, code, 0)?,
                )?),
            };

            out_states.push(registers);
            parameter_registers.push(parameter_regs);
            blocks.push(NumericBlock {
                predecessors: raw.predecessors.clone(),
                successors: raw.successors.clone(),
                parameters,
                successor_arguments: vec![Vec::new(); raw.successors.len()],
                nodes: block_nodes,
                terminator,
            });
        }

        for predecessor in 0..blocks.len() {
            for edge in 0..blocks[predecessor].successors.len() {
                let successor = blocks[predecessor].successors[edge];
                blocks[predecessor].successor_arguments[edge] = parameter_registers[successor]
                    .iter()
                    .map(
                        |&register| match out_states[predecessor][usize::from(register)] {
                            RegisterState::Value(value) => Some(value),
                            RegisterState::Unset | RegisterState::Undefined => None,
                        },
                    )
                    .collect::<Option<Vec<_>>>()?;
            }
        }

        for (predecessor, block) in blocks.iter().enumerate() {
            for (edge, &successor) in block.successors.iter().enumerate() {
                if successor > predecessor {
                    continue;
                }
                frame_states.push(NumericFrameState {
                    point: NumericFramePoint::Backedge { predecessor, edge },
                    function_id: code.id,
                    byte_pc: view.instructions.get(raw_blocks[successor].start)?.byte_pc,
                    slots: out_states[predecessor]
                        .iter()
                        .copied()
                        .zip(live_in[successor].iter().copied())
                        .map(|(state, live)| match (state, live) {
                            (RegisterState::Value(value), true) => NumericFrameSlot::Value(value),
                            (RegisterState::Unset | RegisterState::Undefined, _)
                            | (RegisterState::Value(_), false) => NumericFrameSlot::Undefined,
                        })
                        .collect(),
                });
            }
        }

        Some(Self {
            nodes,
            blocks,
            frame_states,
            parameter_count,
            register_count,
            arithmetic_op_count,
        })
    }
}

fn build_raw_blocks(view: &JitCompileSnapshot) -> Option<Vec<RawBlock>> {
    let code = view.code_block.as_ref();
    let starts = code.block_starts();
    if starts.first().copied() != Some(0) {
        return None;
    }
    let by_pc = starts
        .iter()
        .enumerate()
        .map(|(index, &pc)| (pc, index))
        .collect::<BTreeMap<_, _>>();
    let mut blocks = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate() {
        let end = starts
            .get(index + 1)
            .copied()
            .unwrap_or(view.instructions.len() as u32);
        let terminal_pc = end.checked_sub(1)?;
        let instruction = view.instructions.get(terminal_pc as usize)?;
        let op = instruction.op(code);
        let (successors, terminator) = match op {
            Op::Jump => (
                vec![target_block(instruction, code, terminal_pc, &by_pc)?],
                RawTerminator::Jump,
            ),
            Op::JumpIfTrue | Op::JumpIfFalse => (
                vec![
                    target_block(instruction, code, terminal_pc, &by_pc)?,
                    *by_pc.get(&end)?,
                ],
                RawTerminator::Branch {
                    when_true: op == Op::JumpIfTrue,
                },
            ),
            Op::ReturnValue => (Vec::new(), RawTerminator::Return),
            _ => (vec![*by_pc.get(&end)?], RawTerminator::Jump),
        };
        blocks.push(RawBlock {
            start: start as usize,
            end: end as usize,
            predecessors: Vec::new(),
            successors,
            terminator,
        });
    }
    for predecessor in 0..blocks.len() {
        for successor in blocks[predecessor].successors.clone() {
            blocks.get_mut(successor)?.predecessors.push(predecessor);
        }
    }
    if blocks
        .iter()
        .skip(1)
        .any(|block| block.predecessors.is_empty())
    {
        return None;
    }
    Some(blocks)
}

fn build_liveness(
    view: &JitCompileSnapshot,
    blocks: &[RawBlock],
    register_count: u16,
) -> Option<Vec<Vec<bool>>> {
    let code = view.code_block.as_ref();
    let width = usize::from(register_count);
    let mut uses = vec![vec![false; width]; blocks.len()];
    let mut definitions = vec![vec![false; width]; blocks.len()];
    for (block_index, block) in blocks.iter().enumerate() {
        for pc in block.start..block.end {
            let instruction = view.instructions.get(pc)?;
            let (reads, writes) = instruction_accesses(instruction, code)?;
            for read in reads {
                let read = usize::from(read);
                if !definitions[block_index][read] {
                    uses[block_index][read] = true;
                }
            }
            for write in writes {
                definitions[block_index][usize::from(write)] = true;
            }
        }
    }

    let mut live_in = vec![vec![false; width]; blocks.len()];
    loop {
        let mut changed = false;
        for block_index in (0..blocks.len()).rev() {
            let mut next = uses[block_index].clone();
            for &successor in &blocks[block_index].successors {
                for register in 0..width {
                    if live_in[successor][register] && !definitions[block_index][register] {
                        next[register] = true;
                    }
                }
            }
            if next != live_in[block_index] {
                live_in[block_index] = next;
                changed = true;
            }
        }
        if !changed {
            return Some(live_in);
        }
    }
}

fn build_instruction_liveness(
    view: &JitCompileSnapshot,
    blocks: &[RawBlock],
    block_live_in: &[Vec<bool>],
    register_count: u16,
) -> Option<Vec<Vec<bool>>> {
    let code = view.code_block.as_ref();
    let width = usize::from(register_count);
    let mut instruction_live_in = vec![vec![false; width]; view.instructions.len()];
    for block in blocks {
        let mut live = vec![false; width];
        for &successor in &block.successors {
            for (register, &successor_live) in block_live_in[successor].iter().enumerate() {
                live[register] |= successor_live;
            }
        }
        for pc in (block.start..block.end).rev() {
            let instruction = view.instructions.get(pc)?;
            let (reads, writes) = instruction_accesses(instruction, code)?;
            for write in writes {
                live[usize::from(write)] = false;
            }
            for read in reads {
                live[usize::from(read)] = true;
            }
            instruction_live_in[pc] = live.clone();
        }
    }
    Some(instruction_live_in)
}

fn instruction_accesses(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
) -> Option<(Vec<u16>, Vec<u16>)> {
    match instruction.op(code) {
        Op::StoreLocal => Some((
            vec![register(instruction, code, 0)?],
            vec![local_index(instruction, code, 1)?],
        )),
        Op::LoadLocal => Some((
            vec![local_index(instruction, code, 1)?],
            vec![register(instruction, code, 0)?],
        )),
        Op::LoadUndefined | Op::LoadInt32 | Op::LoadNumber => {
            Some((Vec::new(), vec![register(instruction, code, 0)?]))
        }
        Op::ToPrimitive | Op::ToNumeric | Op::Neg | Op::Increment | Op::BitwiseNot => Some((
            vec![register(instruction, code, 1)?],
            vec![register(instruction, code, 0)?],
        )),
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::BitwiseAnd
        | Op::BitwiseOr
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
        | Op::LessThan => Some((
            vec![
                register(instruction, code, 1)?,
                register(instruction, code, 2)?,
            ],
            vec![register(instruction, code, 0)?],
        )),
        Op::AddImm
        | Op::SubImm
        | Op::BitwiseAndImm
        | Op::LessThanImm
        | Op::EqualImm
        | Op::NotEqualImm => Some((
            vec![register(instruction, code, 1)?],
            vec![register(instruction, code, 0)?],
        )),
        Op::JumpIfTrue | Op::JumpIfFalse => {
            Some((vec![register(instruction, code, 1)?], Vec::new()))
        }
        Op::ReturnValue => Some((vec![register(instruction, code, 0)?], Vec::new())),
        Op::Jump => Some((Vec::new(), Vec::new())),
        _ => None,
    }
}

fn target_block(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    pc: u32,
    by_pc: &BTreeMap<u32, usize>,
) -> Option<usize> {
    let target = i64::from(pc) + 1 + i64::from(instruction.imm32(code, 0)?);
    u32::try_from(target)
        .ok()
        .and_then(|target| by_pc.get(&target).copied())
}

fn merge_predecessors(
    predecessors: &[usize],
    live_in: &[bool],
    out_states: &[Vec<RegisterState>],
    nodes: &mut Vec<NumericNode>,
) -> Option<(Vec<RegisterState>, Vec<NumericValue>, Vec<u16>)> {
    let first = out_states.get(*predecessors.first()?)?.clone();
    let mut merged = first.clone();
    let mut parameters = Vec::new();
    let mut parameter_registers = Vec::new();
    for (register, merged_state) in merged.iter_mut().enumerate() {
        if !live_in.get(register).copied().unwrap_or(false) {
            *merged_state = RegisterState::Unset;
            continue;
        }
        let states = predecessors
            .iter()
            .map(|&predecessor| out_states.get(predecessor)?.get(register).copied())
            .collect::<Option<Vec<_>>>()?;
        if states.iter().all(|&state| state == states[0]) {
            continue;
        }
        let mut value_type = None;
        for state in states {
            let RegisterState::Value(value) = state else {
                return None;
            };
            let current = nodes.get(value.0)?.value_type();
            if value_type.is_some_and(|expected| expected != current) {
                return None;
            }
            value_type = Some(current);
        }
        let parameter = push(nodes, NumericNode::BlockParameter(value_type?));
        *merged_state = RegisterState::Value(parameter);
        parameters.push(parameter);
        parameter_registers.push(u16::try_from(register).ok()?);
    }
    Some((merged, parameters, parameter_registers))
}

fn force_loop_parameters(
    registers: &mut [RegisterState],
    parameters: &mut Vec<NumericValue>,
    parameter_registers: &mut Vec<u16>,
    nodes: &mut Vec<NumericNode>,
    live_in: &[bool],
) -> Option<()> {
    for (register, state) in registers.iter_mut().enumerate() {
        if !live_in.get(register).copied().unwrap_or(false) {
            *state = RegisterState::Unset;
            continue;
        }
        let RegisterState::Value(value) = *state else {
            continue;
        };
        if parameter_registers.contains(&u16::try_from(register).ok()?) {
            continue;
        }
        let value_type = nodes.get(value.0)?.value_type();
        let parameter = push(nodes, NumericNode::BlockParameter(value_type));
        *state = RegisterState::Value(parameter);
        parameters.push(parameter);
        parameter_registers.push(u16::try_from(register).ok()?);
    }
    Some(())
}

fn lower_instruction(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    registers: &mut [RegisterState],
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    arithmetic_op_count: &mut usize,
    live_in: &[bool],
    frame_states: &mut Vec<NumericFrameState>,
    function_id: u32,
) -> Option<()> {
    let op = instruction.op(code);
    let node = match op {
        Op::StoreLocal => {
            let value = read_state(registers, register(instruction, code, 0)?)?;
            write(registers, local_index(instruction, code, 1)?, value)?;
            return Some(());
        }
        Op::LoadLocal => {
            let value = read_state(registers, local_index(instruction, code, 1)?)?;
            write(registers, register(instruction, code, 0)?, value)?;
            return Some(());
        }
        Op::LoadUndefined => {
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Undefined,
            )?;
            return Some(());
        }
        Op::LoadInt32 => NumericNode::IntegerConstant(instruction.imm32(code, 1)?),
        Op::LoadNumber => {
            instruction.const_index(code, 1)?;
            NumericNode::Constant(instruction.load_number?)
        }
        Op::ToPrimitive => {
            instruction.const_index(code, 2)?;
            let value = read_number(registers, nodes, register(instruction, code, 1)?)?;
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::ToNumeric => {
            let value = read_number(registers, nodes, register(instruction, code, 1)?)?;
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::Add | Op::Sub | Op::Mul | Op::Div => {
            if !instruction.arith_feedback().is_numeric_only() {
                return None;
            }
            let left = read_number(registers, nodes, register(instruction, code, 1)?)?;
            let right = read_number(registers, nodes, register(instruction, code, 2)?)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            if matches!(op, Op::Add | Op::Sub)
                && instruction.arith_feedback().is_int32_only()
                && value_type(nodes, left)? == NumericType::Int32
                && value_type(nodes, right)? == NumericType::Int32
            {
                if op == Op::Add {
                    NumericNode::IntegerAdd(left, right)
                } else {
                    NumericNode::IntegerSub(left, right)
                }
            } else {
                let left = widen_to_number(left, nodes, block_nodes)?;
                let right = widen_to_number(right, nodes, block_nodes)?;
                match op {
                    Op::Add => NumericNode::Add(left, right),
                    Op::Sub => NumericNode::Sub(left, right),
                    Op::Mul => NumericNode::Mul(left, right),
                    Op::Div => NumericNode::Div(left, right),
                    _ => unreachable!("matched numeric binary operation"),
                }
            }
        }
        Op::Increment | Op::AddImm | Op::SubImm | Op::BitwiseAndImm => {
            if !instruction.arith_feedback().is_int32_only() {
                return None;
            }
            let source = read_int32(registers, nodes, register(instruction, code, 1)?)?;
            let immediate = instruction.imm32(code, 2)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            match op {
                Op::Increment | Op::AddImm => NumericNode::IntegerAddImmediate(source, immediate),
                Op::SubImm => NumericNode::IntegerSubImmediate(source, immediate),
                Op::BitwiseAndImm => NumericNode::IntegerAndImmediate(source, immediate),
                _ => unreachable!("matched immediate int32 operation"),
            }
        }
        Op::LessThanImm | Op::EqualImm | Op::NotEqualImm => {
            if !instruction.arith_feedback().is_int32_only() {
                return None;
            }
            let source = read_int32(registers, nodes, register(instruction, code, 1)?)?;
            let immediate = instruction.imm32(code, 2)?;
            match op {
                Op::LessThanImm => NumericNode::IntegerLessThanImmediate(source, immediate),
                Op::EqualImm => NumericNode::IntegerEqualImmediate(source, immediate),
                Op::NotEqualImm => NumericNode::IntegerNotEqualImmediate(source, immediate),
                _ => unreachable!("matched immediate int32 comparison"),
            }
        }
        Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor | Op::Shl | Op::Shr => {
            let left = read_int32(registers, nodes, register(instruction, code, 1)?)?;
            let right = read_int32(registers, nodes, register(instruction, code, 2)?)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            match op {
                Op::BitwiseAnd => NumericNode::IntegerAnd(left, right),
                Op::BitwiseOr => NumericNode::IntegerOr(left, right),
                Op::BitwiseXor => NumericNode::IntegerXor(left, right),
                Op::Shl => NumericNode::IntegerShiftLeft(left, right),
                Op::Shr => NumericNode::IntegerShiftRight(left, right),
                _ => unreachable!("matched binary int32 operation"),
            }
        }
        Op::BitwiseNot => {
            let source = read_int32(registers, nodes, register(instruction, code, 1)?)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            NumericNode::IntegerNot(source)
        }
        Op::Neg => {
            if !instruction.arith_feedback().is_numeric_only() {
                return None;
            }
            let source = read_number(registers, nodes, register(instruction, code, 1)?)?;
            let source = widen_to_number(source, nodes, block_nodes)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            NumericNode::Neg(source)
        }
        Op::LessThan => {
            if !instruction.arith_feedback().is_numeric_only() {
                return None;
            }
            let left = read_number(registers, nodes, register(instruction, code, 1)?)?;
            let right = read_number(registers, nodes, register(instruction, code, 2)?)?;
            NumericNode::LessThan(
                widen_to_number(left, nodes, block_nodes)?,
                widen_to_number(right, nodes, block_nodes)?,
            )
        }
        _ => return None,
    };
    let destination = register(instruction, code, 0)?;
    let value = push(nodes, node);
    block_nodes.push(value);
    if matches!(
        node,
        NumericNode::IntegerAdd(..)
            | NumericNode::IntegerSub(..)
            | NumericNode::IntegerAddImmediate(..)
            | NumericNode::IntegerSubImmediate(..)
    ) {
        frame_states.push(NumericFrameState {
            point: NumericFramePoint::Node(value),
            function_id,
            byte_pc: instruction.byte_pc,
            slots: registers
                .iter()
                .copied()
                .zip(live_in.iter().copied())
                .map(|(state, live)| match (state, live) {
                    (RegisterState::Value(value), true) => NumericFrameSlot::Value(value),
                    (RegisterState::Unset | RegisterState::Undefined, _)
                    | (RegisterState::Value(_), false) => NumericFrameSlot::Undefined,
                })
                .collect(),
        });
    }
    write(registers, destination, RegisterState::Value(value))
}

fn push(nodes: &mut Vec<NumericNode>, node: NumericNode) -> NumericValue {
    let value = NumericValue(nodes.len());
    nodes.push(node);
    value
}

fn register(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    index: usize,
) -> Option<u16> {
    match instruction.operand(code, index) {
        Some(Operand::Register(register)) => Some(register),
        _ => None,
    }
}

fn local_index(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    index: usize,
) -> Option<u16> {
    u16::try_from(instruction.imm32(code, index)?).ok()
}

fn read_state(registers: &[RegisterState], register: u16) -> Option<RegisterState> {
    match registers.get(usize::from(register)).copied()? {
        RegisterState::Unset => None,
        value => Some(value),
    }
}

fn read_number(
    registers: &[RegisterState],
    nodes: &[NumericNode],
    register: u16,
) -> Option<NumericValue> {
    let RegisterState::Value(value) = read_state(registers, register)? else {
        return None;
    };
    matches!(
        nodes.get(value.0)?.value_type(),
        NumericType::Int32 | NumericType::Number
    )
    .then_some(value)
}

fn read_int32(
    registers: &[RegisterState],
    nodes: &[NumericNode],
    register: u16,
) -> Option<NumericValue> {
    let RegisterState::Value(value) = read_state(registers, register)? else {
        return None;
    };
    (value_type(nodes, value)? == NumericType::Int32).then_some(value)
}

fn value_type(nodes: &[NumericNode], value: NumericValue) -> Option<NumericType> {
    nodes.get(value.0).copied().map(NumericNode::value_type)
}

fn widen_to_number(
    value: NumericValue,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
) -> Option<NumericValue> {
    match value_type(nodes, value)? {
        NumericType::Number => Some(value),
        NumericType::Int32 => {
            let widened = push(nodes, NumericNode::WidenInt32(value));
            block_nodes.push(widened);
            Some(widened)
        }
        NumericType::Boolean => None,
    }
}

fn read_boolean(
    registers: &[RegisterState],
    nodes: &[NumericNode],
    register: u16,
) -> Option<NumericValue> {
    let RegisterState::Value(value) = read_state(registers, register)? else {
        return None;
    };
    (nodes.get(value.0)?.value_type() == NumericType::Boolean).then_some(value)
}

fn write(registers: &mut [RegisterState], register: u16, value: RegisterState) -> Option<()> {
    *registers.get_mut(usize::from(register))? = value;
    Some(())
}
