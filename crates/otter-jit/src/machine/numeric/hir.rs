//! Typed scalar HIR and control-flow construction.
//!
//! # Contents
//! - [`NumericFunction`] — bounded numeric SSA graph with explicit blocks.
//! - [`NumericBlock`] and [`NumericTerminator`] — predecessor/successor edges,
//!   block parameters, edge arguments, branches, and returns.
//! - [`NumericNode`] — tagged/scalar parameters, constants, moves, coercions,
//!   arithmetic, comparison, and typed plain/method calls.
//!
//! # Invariants
//! - Parameters remain tagged unless their uses prove a numeric representation;
//!   inferred Number/Int32 parameters are guarded before effects.
//! - Parameters outside the exact entry live-in set have no HIR value, load,
//!   guard, or allocator interval.
//! - Reentrant calls require one VM-planned direct target and carry an exact
//!   pre-call FrameState. Guarded methods additionally retain the VM-baked
//!   receiver/prototype/slot identity. Supported catch regions become explicit
//!   exceptional CFG edges whose landing state receives the thrown value.
//! - All other tagged coercions/equality use declared leaf stubs; primitive
//!   string concatenation uses the allocating stub family.
//! - Register merges become typed block parameters. Only loop-header OSR
//!   metadata retains the aligned VM-register sources needed at the entry ABI.
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
    Tagged,
    Int32,
    Uint32,
    Number,
    Boolean,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum NumericNode {
    Parameter {
        register: u16,
        value_type: NumericType,
    },
    BlockParameter(NumericType),
    TaggedConstant(u64),
    This,
    TaggedToBoolean(NumericValue),
    TaggedStrictEqual(NumericValue, NumericValue),
    TaggedStringConcat(NumericValue, NumericValue),
    DirectCall {
        source: NumericValue,
        target: u16,
        argument_start: u16,
        argument_count: u8,
        logical_pc: u32,
        byte_pc: u32,
        exceptional_edge: Option<u16>,
    },
    IntegerConstant(i32),
    BooleanConstant(bool),
    Constant(f64),
    WidenInt32(NumericValue),
    WidenUint32(NumericValue),
    FloatToInt32(NumericValue),
    BooleanToInt32(NumericValue),
    IntegerAdd(NumericValue, NumericValue),
    IntegerSub(NumericValue, NumericValue),
    IntegerMul(NumericValue, NumericValue),
    IntegerNeg(NumericValue),
    IntegerAddImmediate(NumericValue, i32),
    IntegerSubImmediate(NumericValue, i32),
    IntegerAnd(NumericValue, NumericValue),
    IntegerOr(NumericValue, NumericValue),
    IntegerXor(NumericValue, NumericValue),
    IntegerShiftLeft(NumericValue, NumericValue),
    IntegerShiftRight(NumericValue, NumericValue),
    IntegerShiftRightLogical(NumericValue, NumericValue),
    IntegerNot(NumericValue),
    IntegerAndImmediate(NumericValue, i32),
    IntegerLessThanImmediate(NumericValue, i32),
    IntegerEqualImmediate(NumericValue, i32),
    IntegerNotEqualImmediate(NumericValue, i32),
    IntegerEqual(NumericValue, NumericValue),
    IntegerNotEqual(NumericValue, NumericValue),
    IntegerLessThan(NumericValue, NumericValue),
    IntegerLessEqual(NumericValue, NumericValue),
    IntegerGreaterThan(NumericValue, NumericValue),
    IntegerGreaterEqual(NumericValue, NumericValue),
    Add(NumericValue, NumericValue),
    Sub(NumericValue, NumericValue),
    Mul(NumericValue, NumericValue),
    Div(NumericValue, NumericValue),
    Rem(NumericValue, NumericValue),
    Pow(NumericValue, NumericValue),
    Neg(NumericValue),
    IntegerToBoolean(NumericValue),
    FloatToBoolean(NumericValue),
    BooleanNot(NumericValue),
    LessThan(NumericValue, NumericValue),
    Equal(NumericValue, NumericValue),
    NotEqual(NumericValue, NumericValue),
    LessEqual(NumericValue, NumericValue),
    GreaterThan(NumericValue, NumericValue),
    GreaterEqual(NumericValue, NumericValue),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NumericDirectCallKind {
    Plain,
    Method(otter_vm::jit::JitMethodGuard),
    Construct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NumericDirectCallTarget {
    pub(super) kind: NumericDirectCallKind,
    pub(super) callee: otter_vm::JitDirectCallee,
}

impl NumericNode {
    pub(super) const fn value_type(self) -> NumericType {
        match self {
            Self::TaggedConstant(..)
            | Self::This
            | Self::TaggedStringConcat(..)
            | Self::DirectCall { .. }
            | Self::BlockParameter(NumericType::Tagged) => NumericType::Tagged,
            Self::IntegerConstant(..)
            | Self::FloatToInt32(..)
            | Self::BooleanToInt32(..)
            | Self::IntegerAdd(..)
            | Self::IntegerSub(..)
            | Self::IntegerMul(..)
            | Self::IntegerNeg(..)
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
            Self::IntegerShiftRightLogical(..) | Self::BlockParameter(NumericType::Uint32) => {
                NumericType::Uint32
            }
            Self::LessThan(..)
            | Self::Equal(..)
            | Self::NotEqual(..)
            | Self::LessEqual(..)
            | Self::GreaterThan(..)
            | Self::GreaterEqual(..)
            | Self::IntegerEqual(..)
            | Self::IntegerNotEqual(..)
            | Self::IntegerLessThan(..)
            | Self::IntegerLessEqual(..)
            | Self::IntegerGreaterThan(..)
            | Self::IntegerGreaterEqual(..)
            | Self::TaggedToBoolean(..)
            | Self::TaggedStrictEqual(..)
            | Self::IntegerToBoolean(..)
            | Self::FloatToBoolean(..)
            | Self::BooleanNot(..)
            | Self::IntegerLessThanImmediate(..)
            | Self::IntegerEqualImmediate(..)
            | Self::IntegerNotEqualImmediate(..)
            | Self::BlockParameter(NumericType::Boolean) => NumericType::Boolean,
            Self::BooleanConstant(..) => NumericType::Boolean,
            Self::Parameter { value_type, .. } => value_type,
            Self::BlockParameter(NumericType::Number)
            | Self::Constant(..)
            | Self::WidenInt32(..)
            | Self::WidenUint32(..)
            | Self::Add(..)
            | Self::Sub(..)
            | Self::Mul(..)
            | Self::Div(..)
            | Self::Rem(..)
            | Self::Pow(..)
            | Self::Neg(..) => NumericType::Number,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisterState {
    Unset,
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
    pub(super) logical_pc: u32,
    pub(super) predecessors: Vec<usize>,
    pub(super) successors: Vec<usize>,
    pub(super) parameters: Vec<NumericValue>,
    pub(super) parameter_registers: Vec<u16>,
    pub(super) successor_arguments: Vec<Vec<NumericValue>>,
    pub(super) nodes: Vec<NumericValue>,
    pub(super) terminator: NumericTerminator,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NumericFunction {
    pub(super) function_id: u32,
    pub(super) nodes: Vec<NumericNode>,
    pub(super) blocks: Vec<NumericBlock>,
    pub(super) frame_states: Vec<NumericFrameState>,
    pub(super) direct_call_targets: Vec<NumericDirectCallTarget>,
    pub(super) direct_call_arguments: Vec<NumericValue>,
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
    ReturnValue,
    ReturnUndefined,
}

#[derive(Debug, Clone)]
struct RawBlock {
    start: usize,
    end: usize,
    predecessors: Vec<usize>,
    successors: Vec<usize>,
    exceptional_edge: Option<usize>,
    exception_register: Option<u16>,
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
        {
            return None;
        }
        if code
            .control_flow()
            .exception_regions()
            .iter()
            .any(|region| region.catch_pc.is_none() || region.finally_pc.is_some())
        {
            return None;
        }

        let raw_blocks = build_raw_blocks(view)?;
        let parameter_types =
            infer_parameter_types(view, &raw_blocks, parameter_count, register_count)?;
        let live_in = build_liveness(view, &raw_blocks, register_count)?;
        let instruction_live_in =
            build_instruction_liveness(view, &raw_blocks, &live_in, register_count)?;
        let mut nodes = Vec::with_capacity(view.instructions.len() + register_count as usize);
        let mut entry = vec![RegisterState::Unset; usize::from(register_count)];
        let mut entry_nodes = Vec::with_capacity(parameter_count as usize);
        for parameter in 0..parameter_count {
            if !live_in[0][usize::from(parameter)] {
                continue;
            }
            let value = push(
                &mut nodes,
                NumericNode::Parameter {
                    register: parameter,
                    value_type: parameter_types[usize::from(parameter)],
                },
            );
            entry[usize::from(parameter)] = RegisterState::Value(value);
            entry_nodes.push(value);
        }

        let mut blocks = Vec::with_capacity(raw_blocks.len());
        let mut out_states = Vec::<Vec<RegisterState>>::with_capacity(raw_blocks.len());
        let mut exceptional_out_states =
            Vec::<Option<Vec<RegisterState>>>::with_capacity(raw_blocks.len());
        let mut arithmetic_op_count = 0usize;
        let mut frame_states = Vec::new();
        let mut direct_call_targets = Vec::new();
        let mut direct_call_arguments = Vec::new();

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
                    block_index,
                    &raw_blocks,
                    &live_in[block_index],
                    &out_states,
                    &exceptional_out_states,
                    &mut nodes,
                )?
            } else {
                merge_predecessors(
                    &raw.predecessors,
                    block_index,
                    &raw_blocks,
                    &live_in[block_index],
                    &out_states,
                    &exceptional_out_states,
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
            let mut exceptional_pre_state = None;
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
                        Op::Jump
                            | Op::JumpIfTrue
                            | Op::JumpIfFalse
                            | Op::Return
                            | Op::ReturnValue
                            | Op::ReturnUndefined
                    )
                {
                    break;
                }
                if pc == terminal_pc && raw.exceptional_edge.is_some() {
                    exceptional_pre_state = Some(registers.clone());
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
                    u32::try_from(pc).ok()?,
                    &view.direct_callees,
                    &view.direct_constructs,
                    &view.direct_methods,
                    &mut direct_call_targets,
                    &mut direct_call_arguments,
                    (pc == terminal_pc)
                        .then_some(raw.exceptional_edge)
                        .flatten(),
                )?;
            }

            let terminal = &view.instructions[terminal_pc];
            let terminator = match raw.terminator {
                RawTerminator::Jump => NumericTerminator::Jump,
                RawTerminator::Branch { when_true } => {
                    let source = read_value(&registers, register(terminal, code, 1)?)?;
                    let condition = to_boolean(source, &mut nodes, &mut block_nodes)?;
                    if matches!(nodes[condition.0], NumericNode::TaggedToBoolean(..)) {
                        push_frame_state(
                            &mut frame_states,
                            NumericFramePoint::Node(condition),
                            code.id,
                            terminal.byte_pc,
                            &registers,
                            &instruction_live_in[terminal_pc],
                        );
                    }
                    NumericTerminator::Branch {
                        condition,
                        when_true,
                    }
                }
                RawTerminator::ReturnValue => {
                    NumericTerminator::Return(read_value(&registers, register(terminal, code, 0)?)?)
                }
                RawTerminator::ReturnUndefined => {
                    let value = push(
                        &mut nodes,
                        NumericNode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                    );
                    block_nodes.push(value);
                    NumericTerminator::Return(value)
                }
            };

            let exceptional_state = if let (Some(mut state), Some(exception_register)) =
                (exceptional_pre_state, raw.exception_register)
            {
                let destination = register(terminal, code, 0)?;
                let result = read_state(&registers, destination)?;
                state[usize::from(destination)] = RegisterState::Unset;
                state[usize::from(exception_register)] = result;
                Some(state)
            } else {
                None
            };
            out_states.push(registers);
            exceptional_out_states.push(exceptional_state);
            blocks.push(NumericBlock {
                logical_pc: u32::try_from(raw.start).ok()?,
                predecessors: raw.predecessors.clone(),
                successors: raw.successors.clone(),
                parameters,
                parameter_registers: parameter_regs,
                successor_arguments: vec![Vec::new(); raw.successors.len()],
                nodes: block_nodes,
                terminator,
            });
        }

        for predecessor in 0..blocks.len() {
            for edge in 0..blocks[predecessor].successors.len() {
                let successor = blocks[predecessor].successors[edge];
                let successor_registers = blocks[successor].parameter_registers.clone();
                let edge_state = edge_state(
                    predecessor,
                    edge,
                    &raw_blocks,
                    &out_states,
                    &exceptional_out_states,
                )?;
                let arguments = successor_registers
                    .iter()
                    .map(|&register| match edge_state[usize::from(register)] {
                        RegisterState::Value(value) => Some(value),
                        RegisterState::Unset => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                if arguments.iter().zip(&blocks[successor].parameters).any(
                    |(&argument, &parameter)| {
                        value_type(&nodes, argument) != value_type(&nodes, parameter)
                    },
                ) {
                    return None;
                }
                blocks[predecessor].successor_arguments[edge] = arguments;
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
                            (RegisterState::Unset, _) | (RegisterState::Value(_), false) => {
                                NumericFrameSlot::Undefined
                            }
                        })
                        .collect(),
                });
            }
        }

        Some(Self {
            function_id: code.id,
            nodes,
            blocks,
            frame_states,
            direct_call_targets,
            direct_call_arguments,
            parameter_count,
            register_count,
            arithmetic_op_count,
        })
    }
}

fn infer_parameter_types(
    view: &JitCompileSnapshot,
    blocks: &[RawBlock],
    parameter_count: u16,
    register_count: u16,
) -> Option<Vec<NumericType>> {
    let code = view.code_block.as_ref();
    let width = usize::from(register_count);
    let mut entry = vec![0_u16; width];
    for parameter in 0..parameter_count {
        entry[usize::from(parameter)] = 1_u16.checked_shl(u32::from(parameter))?;
    }
    let mut out_origins = vec![vec![0_u16; width]; blocks.len()];
    let mut int32_parameters = 0_u16;
    let mut number_parameters = 0_u16;

    loop {
        let mut changed = false;
        for (block_index, block) in blocks.iter().enumerate() {
            let mut origins = if block_index == 0 {
                entry.clone()
            } else {
                let mut merged = vec![0_u16; width];
                for &predecessor in &block.predecessors {
                    for (destination, &source) in merged.iter_mut().zip(&out_origins[predecessor]) {
                        *destination |= source;
                    }
                }
                merged
            };
            for pc in block.start..block.end {
                infer_instruction_parameters(
                    view.instructions.get(pc)?,
                    code,
                    &mut origins,
                    &mut int32_parameters,
                    &mut number_parameters,
                )?;
            }
            if origins != out_origins[block_index] {
                out_origins[block_index] = origins;
                changed = true;
            }
        }
        if !changed {
            return Some(
                (0..parameter_count)
                    .map(|parameter| {
                        let bit = 1_u16 << parameter;
                        if int32_parameters & bit != 0 {
                            NumericType::Int32
                        } else if number_parameters & bit != 0 {
                            NumericType::Number
                        } else {
                            NumericType::Tagged
                        }
                    })
                    .collect(),
            );
        }
    }
}

fn infer_instruction_parameters(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    origins: &mut [u16],
    int32_parameters: &mut u16,
    number_parameters: &mut u16,
) -> Option<()> {
    let read = |register: u16| origins.get(usize::from(register)).copied();
    let op = instruction.op(code);
    match op {
        Op::StoreLocal => {
            let source = read(register(instruction, code, 0)?)?;
            *origins.get_mut(usize::from(local_index(instruction, code, 1)?))? = source;
        }
        Op::LoadLocal => {
            let source = read(local_index(instruction, code, 1)?)?;
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = source;
        }
        Op::LoadUndefined
        | Op::LoadNull
        | Op::LoadTrue
        | Op::LoadFalse
        | Op::LoadInt32
        | Op::LoadNumber
        | Op::LoadThis => {
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::Call | Op::New => {
            let count = usize::try_from(instruction.const_index(code, 2)?).ok()?;
            for index in 0..count {
                let _ = read(register(instruction, code, 3 + index)?)?;
            }
            let _ = read(register(instruction, code, 1)?)?;
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::CallMethodValue => {
            let count = usize::try_from(instruction.const_index(code, 3)?).ok()?;
            let _ = instruction.const_index(code, 2)?;
            let _ = read(register(instruction, code, 1)?)?;
            for index in 0..count {
                let _ = read(register(instruction, code, 4 + index)?)?;
            }
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::ToPrimitive | Op::ToNumeric | Op::ToNumber => {
            let source = read(register(instruction, code, 1)?)?;
            *number_parameters |= source;
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = source;
        }
        Op::Neg | Op::Increment | Op::AddImm | Op::SubImm => {
            let source = read(register(instruction, code, 1)?)?;
            *number_parameters |= source;
            if instruction.arith_feedback().is_int32_only() {
                *int32_parameters |= source;
                *origins.get_mut(usize::from(register(instruction, code, 0)?))? = source;
            } else {
                *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
            }
        }
        Op::Add
            if instruction
                .arith_feedback()
                .is_primitive_string_concat_only() =>
        {
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::Add | Op::Sub | Op::Mul => {
            let left = read(register(instruction, code, 1)?)?;
            let right = read(register(instruction, code, 2)?)?;
            *number_parameters |= left | right;
            let destination = usize::from(register(instruction, code, 0)?);
            if instruction.arith_feedback().is_int32_only() {
                *int32_parameters |= left | right;
                *origins.get_mut(destination)? = left | right;
            } else {
                *origins.get_mut(destination)? = 0;
            }
        }
        Op::Equal | Op::NotEqual => {
            let inputs =
                read(register(instruction, code, 1)?)? | read(register(instruction, code, 2)?)?;
            if instruction.arith_feedback().is_numeric_only() {
                *number_parameters |= inputs;
                if instruction.arith_feedback().is_int32_only() {
                    *int32_parameters |= inputs;
                }
            }
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
            let inputs =
                read(register(instruction, code, 1)?)? | read(register(instruction, code, 2)?)?;
            *number_parameters |= inputs;
            if instruction.arith_feedback().is_int32_only() {
                *int32_parameters |= inputs;
            }
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::LessThanImm | Op::EqualImm | Op::NotEqualImm => {
            let source = read(register(instruction, code, 1)?)?;
            *number_parameters |= source;
            if instruction.arith_feedback().is_int32_only() {
                *int32_parameters |= source;
            }
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::Div
        | Op::Rem
        | Op::Pow
        | Op::BitwiseAnd
        | Op::BitwiseOr
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
        | Op::Ushr => {
            *number_parameters |=
                read(register(instruction, code, 1)?)? | read(register(instruction, code, 2)?)?;
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::ToBoolean | Op::LogicalNot => {
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::BitwiseNot | Op::BitwiseAndImm => {
            *number_parameters |= read(register(instruction, code, 1)?)?;
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::JumpIfTrue
        | Op::JumpIfFalse
        | Op::EnterTry
        | Op::LeaveTry
        | Op::Return
        | Op::ReturnValue
        | Op::ReturnUndefined
        | Op::Nop
        | Op::Jump => {}
        _ => return None,
    }
    Some(())
}

fn build_raw_blocks(view: &JitCompileSnapshot) -> Option<Vec<RawBlock>> {
    let code = view.code_block.as_ref();
    let mut starts = code
        .block_starts()
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    for (pc, instruction) in view.instructions.iter().enumerate() {
        let pc = u32::try_from(pc).ok()?;
        if matches!(
            instruction.op(code),
            Op::Call | Op::CallMethodValue | Op::New
        ) && code
            .control_flow()
            .enclosing_exception_region(pc)
            .and_then(|region| region.catch_pc)
            .is_some()
            && usize::try_from(pc + 1).ok()? < view.instructions.len()
        {
            starts.insert(pc + 1);
        }
    }
    let starts = starts.into_iter().collect::<Vec<_>>();
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
        let (mut successors, terminator) = match op {
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
            Op::Return | Op::ReturnValue => (Vec::new(), RawTerminator::ReturnValue),
            Op::ReturnUndefined => (Vec::new(), RawTerminator::ReturnUndefined),
            _ => (vec![*by_pc.get(&end)?], RawTerminator::Jump),
        };
        let exceptional = matches!(op, Op::Call | Op::CallMethodValue | Op::New)
            .then(|| code.control_flow().enclosing_exception_region(terminal_pc))
            .flatten()
            .and_then(|region| Some((*by_pc.get(&region.catch_pc?)?, region.exception_register)));
        let (exceptional_edge, exception_register) = if let Some((handler, register)) = exceptional
        {
            let edge = successors.len();
            successors.push(handler);
            (Some(edge), Some(register))
        } else {
            (None, None)
        };
        blocks.push(RawBlock {
            start: start as usize,
            end: end as usize,
            predecessors: Vec::new(),
            successors,
            exceptional_edge,
            exception_register,
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
        if let Some(exception_register) = block.exception_register {
            definitions[block_index][usize::from(exception_register)] = true;
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
            if pc + 1 == block.end
                && let Some(exception_register) = block.exception_register
            {
                live[usize::from(exception_register)] = false;
            }
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
        Op::LoadUndefined
        | Op::LoadNull
        | Op::LoadTrue
        | Op::LoadFalse
        | Op::LoadInt32
        | Op::LoadNumber
        | Op::LoadThis => Some((Vec::new(), vec![register(instruction, code, 0)?])),
        Op::Call | Op::New => {
            let count = usize::try_from(instruction.const_index(code, 2)?).ok()?;
            let mut reads = Vec::with_capacity(count + 1);
            reads.push(register(instruction, code, 1)?);
            for index in 0..count {
                reads.push(register(instruction, code, 3 + index)?);
            }
            Some((reads, vec![register(instruction, code, 0)?]))
        }
        Op::CallMethodValue => {
            let count = usize::try_from(instruction.const_index(code, 3)?).ok()?;
            let _ = instruction.const_index(code, 2)?;
            let mut reads = Vec::with_capacity(count + 1);
            reads.push(register(instruction, code, 1)?);
            for index in 0..count {
                reads.push(register(instruction, code, 4 + index)?);
            }
            Some((reads, vec![register(instruction, code, 0)?]))
        }
        Op::ToPrimitive
        | Op::ToNumeric
        | Op::ToNumber
        | Op::ToBoolean
        | Op::LogicalNot
        | Op::Neg
        | Op::Increment
        | Op::BitwiseNot => Some((
            vec![register(instruction, code, 1)?],
            vec![register(instruction, code, 0)?],
        )),
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Pow
        | Op::BitwiseAnd
        | Op::BitwiseOr
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
        | Op::Ushr
        | Op::Equal
        | Op::NotEqual
        | Op::LessThan
        | Op::LessEq
        | Op::GreaterThan
        | Op::GreaterEq => Some((
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
        Op::Return | Op::ReturnValue => Some((vec![register(instruction, code, 0)?], Vec::new())),
        Op::ReturnUndefined | Op::Nop | Op::Jump | Op::EnterTry | Op::LeaveTry => {
            Some((Vec::new(), Vec::new()))
        }
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
    successor: usize,
    blocks: &[RawBlock],
    live_in: &[bool],
    out_states: &[Vec<RegisterState>],
    exceptional_out_states: &[Option<Vec<RegisterState>>],
    nodes: &mut Vec<NumericNode>,
) -> Option<(Vec<RegisterState>, Vec<NumericValue>, Vec<u16>)> {
    let state = |predecessor: usize| {
        let edge = blocks
            .get(predecessor)?
            .successors
            .iter()
            .position(|&target| target == successor)?;
        edge_state(
            predecessor,
            edge,
            blocks,
            out_states,
            exceptional_out_states,
        )
    };
    let first = state(*predecessors.first()?)?.to_vec();
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
            .map(|&predecessor| state(predecessor)?.get(register).copied())
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

fn edge_state<'a>(
    predecessor: usize,
    edge: usize,
    blocks: &[RawBlock],
    out_states: &'a [Vec<RegisterState>],
    exceptional_out_states: &'a [Option<Vec<RegisterState>>],
) -> Option<&'a [RegisterState]> {
    if blocks.get(predecessor)?.exceptional_edge == Some(edge) {
        exceptional_out_states.get(predecessor)?.as_deref()
    } else {
        out_states.get(predecessor).map(Vec::as_slice)
    }
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
    logical_pc: u32,
    direct_callees: &rustc_hash::FxHashMap<u32, otter_vm::JitDirectCallee>,
    direct_constructs: &rustc_hash::FxHashMap<u32, otter_vm::JitDirectCallee>,
    direct_methods: &rustc_hash::FxHashMap<u32, Vec<otter_vm::jit::JitDirectMethod>>,
    direct_call_targets: &mut Vec<NumericDirectCallTarget>,
    direct_call_arguments: &mut Vec<NumericValue>,
    exceptional_edge: Option<usize>,
) -> Option<()> {
    let op = instruction.op(code);
    let node = match op {
        Op::Nop | Op::EnterTry | Op::LeaveTry => return Some(()),
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
        Op::LoadUndefined => NumericNode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
        Op::LoadNull => NumericNode::TaggedConstant(otter_vm::Value::null().to_bits()),
        Op::LoadThis => NumericNode::This,
        Op::LoadInt32 => NumericNode::IntegerConstant(instruction.imm32(code, 1)?),
        Op::LoadTrue => NumericNode::BooleanConstant(true),
        Op::LoadFalse => NumericNode::BooleanConstant(false),
        Op::LoadNumber => {
            instruction.const_index(code, 1)?;
            NumericNode::Constant(instruction.load_number?)
        }
        Op::Call => {
            let target = NumericDirectCallTarget {
                kind: NumericDirectCallKind::Plain,
                callee: *direct_callees.get(&instruction.byte_pc)?,
            };
            let source = read_value(registers, register(instruction, code, 1)?)?;
            let argument_count = usize::try_from(instruction.const_index(code, 2)?).ok()?;
            let argument_start = u16::try_from(direct_call_arguments.len()).ok()?;
            for index in 0..argument_count {
                direct_call_arguments.push(read_value(
                    registers,
                    register(instruction, code, 3 + index)?,
                )?);
            }
            let target_index = direct_call_targets
                .iter()
                .position(|candidate| *candidate == target)
                .unwrap_or_else(|| {
                    direct_call_targets.push(target);
                    direct_call_targets.len() - 1
                });
            let value = push(
                nodes,
                NumericNode::DirectCall {
                    source,
                    target: u16::try_from(target_index).ok()?,
                    argument_start,
                    argument_count: u8::try_from(argument_count).ok()?,
                    logical_pc,
                    byte_pc: instruction.byte_pc,
                    exceptional_edge: exceptional_edge.map(u16::try_from).transpose().ok()?,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::New => {
            let target = NumericDirectCallTarget {
                kind: NumericDirectCallKind::Construct,
                callee: *direct_constructs.get(&instruction.byte_pc)?,
            };
            let source = read_value(registers, register(instruction, code, 1)?)?;
            let argument_count = usize::try_from(instruction.const_index(code, 2)?).ok()?;
            let argument_start = u16::try_from(direct_call_arguments.len()).ok()?;
            for index in 0..argument_count {
                direct_call_arguments.push(read_value(
                    registers,
                    register(instruction, code, 3 + index)?,
                )?);
            }
            let target_index = direct_call_targets
                .iter()
                .position(|candidate| *candidate == target)
                .unwrap_or_else(|| {
                    direct_call_targets.push(target);
                    direct_call_targets.len() - 1
                });
            let value = push(
                nodes,
                NumericNode::DirectCall {
                    source,
                    target: u16::try_from(target_index).ok()?,
                    argument_start,
                    argument_count: u8::try_from(argument_count).ok()?,
                    logical_pc,
                    byte_pc: instruction.byte_pc,
                    exceptional_edge: exceptional_edge.map(u16::try_from).transpose().ok()?,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::CallMethodValue => {
            let methods = direct_methods.get(&instruction.byte_pc)?;
            let [method] = methods.as_slice() else {
                return None;
            };
            if method.target_count != 1 || method.target_index != 0 {
                return None;
            }
            let _name = instruction.const_index(code, 2)?;
            let target = NumericDirectCallTarget {
                kind: NumericDirectCallKind::Method(method.guard.clone()),
                callee: method.callee,
            };
            let source = read_value(registers, register(instruction, code, 1)?)?;
            let argument_count = usize::try_from(instruction.const_index(code, 3)?).ok()?;
            let argument_start = u16::try_from(direct_call_arguments.len()).ok()?;
            for index in 0..argument_count {
                direct_call_arguments.push(read_value(
                    registers,
                    register(instruction, code, 4 + index)?,
                )?);
            }
            let target_index = direct_call_targets
                .iter()
                .position(|candidate| *candidate == target)
                .unwrap_or_else(|| {
                    direct_call_targets.push(target);
                    direct_call_targets.len() - 1
                });
            let value = push(
                nodes,
                NumericNode::DirectCall {
                    source,
                    target: u16::try_from(target_index).ok()?,
                    argument_start,
                    argument_count: u8::try_from(argument_count).ok()?,
                    logical_pc,
                    byte_pc: instruction.byte_pc,
                    exceptional_edge: exceptional_edge.map(u16::try_from).transpose().ok()?,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
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
        Op::ToNumeric | Op::ToNumber => {
            let value = read_number(registers, nodes, register(instruction, code, 1)?)?;
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::ToBoolean | Op::LogicalNot => {
            let source = read_value(registers, register(instruction, code, 1)?)?;
            let boolean = to_boolean(source, nodes, block_nodes)?;
            if matches!(nodes[boolean.0], NumericNode::TaggedToBoolean(..)) {
                push_frame_state(
                    frame_states,
                    NumericFramePoint::Node(boolean),
                    function_id,
                    instruction.byte_pc,
                    registers,
                    live_in,
                );
            }
            let value = if op == Op::LogicalNot {
                let value = push(nodes, NumericNode::BooleanNot(boolean));
                block_nodes.push(value);
                value
            } else {
                boolean
            };
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::Add
            if instruction
                .arith_feedback()
                .is_primitive_string_concat_only() =>
        {
            let left = read_value(registers, register(instruction, code, 1)?)?;
            let right = read_value(registers, register(instruction, code, 2)?)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            let value = push(nodes, NumericNode::TaggedStringConcat(left, right));
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem | Op::Pow => {
            if !instruction.arith_feedback().is_numeric_only() {
                return None;
            }
            let left = read_number(registers, nodes, register(instruction, code, 1)?)?;
            let right = read_number(registers, nodes, register(instruction, code, 2)?)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            if matches!(op, Op::Add | Op::Sub | Op::Mul)
                && instruction.arith_feedback().is_int32_only()
                && value_type(nodes, left)? == NumericType::Int32
                && value_type(nodes, right)? == NumericType::Int32
            {
                match op {
                    Op::Add => NumericNode::IntegerAdd(left, right),
                    Op::Sub => NumericNode::IntegerSub(left, right),
                    Op::Mul => NumericNode::IntegerMul(left, right),
                    _ => unreachable!("matched checked int32 arithmetic"),
                }
            } else {
                let left = widen_to_number(left, nodes, block_nodes)?;
                let right = widen_to_number(right, nodes, block_nodes)?;
                match op {
                    Op::Add => NumericNode::Add(left, right),
                    Op::Sub => NumericNode::Sub(left, right),
                    Op::Mul => NumericNode::Mul(left, right),
                    Op::Div => NumericNode::Div(left, right),
                    Op::Rem => NumericNode::Rem(left, right),
                    Op::Pow => NumericNode::Pow(left, right),
                    _ => unreachable!("matched numeric binary operation"),
                }
            }
        }
        Op::Increment | Op::AddImm | Op::SubImm => {
            if !instruction.arith_feedback().is_int32_only() {
                return None;
            }
            let source = read_int32(registers, nodes, register(instruction, code, 1)?)?;
            let immediate = instruction.imm32(code, 2)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            match op {
                Op::Increment | Op::AddImm => NumericNode::IntegerAddImmediate(source, immediate),
                Op::SubImm => NumericNode::IntegerSubImmediate(source, immediate),
                _ => unreachable!("matched immediate int32 operation"),
            }
        }
        Op::BitwiseAndImm => {
            let source = read_int32_bits(
                registers,
                nodes,
                block_nodes,
                register(instruction, code, 1)?,
            )?;
            let immediate = instruction.imm32(code, 2)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            NumericNode::IntegerAndImmediate(source, immediate)
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
        Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor | Op::Shl | Op::Shr | Op::Ushr => {
            let left = read_int32_bits(
                registers,
                nodes,
                block_nodes,
                register(instruction, code, 1)?,
            )?;
            let right = read_int32_bits(
                registers,
                nodes,
                block_nodes,
                register(instruction, code, 2)?,
            )?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            match op {
                Op::BitwiseAnd => NumericNode::IntegerAnd(left, right),
                Op::BitwiseOr => NumericNode::IntegerOr(left, right),
                Op::BitwiseXor => NumericNode::IntegerXor(left, right),
                Op::Shl => NumericNode::IntegerShiftLeft(left, right),
                Op::Shr => NumericNode::IntegerShiftRight(left, right),
                Op::Ushr => NumericNode::IntegerShiftRightLogical(left, right),
                _ => unreachable!("matched binary int32 operation"),
            }
        }
        Op::BitwiseNot => {
            let source = read_int32_bits(
                registers,
                nodes,
                block_nodes,
                register(instruction, code, 1)?,
            )?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            NumericNode::IntegerNot(source)
        }
        Op::Neg => {
            if !instruction.arith_feedback().is_numeric_only() {
                return None;
            }
            let source = read_number(registers, nodes, register(instruction, code, 1)?)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            if instruction.arith_feedback().is_int32_only()
                && value_type(nodes, source)? == NumericType::Int32
            {
                NumericNode::IntegerNeg(source)
            } else {
                NumericNode::Neg(widen_to_number(source, nodes, block_nodes)?)
            }
        }
        Op::Equal | Op::NotEqual => {
            if !instruction.arith_feedback().is_numeric_only() {
                let left = read_value(registers, register(instruction, code, 1)?)?;
                let right = read_value(registers, register(instruction, code, 2)?)?;
                let equal = push(nodes, NumericNode::TaggedStrictEqual(left, right));
                block_nodes.push(equal);
                push_frame_state(
                    frame_states,
                    NumericFramePoint::Node(equal),
                    function_id,
                    instruction.byte_pc,
                    registers,
                    live_in,
                );
                let value = if op == Op::NotEqual {
                    let value = push(nodes, NumericNode::BooleanNot(equal));
                    block_nodes.push(value);
                    value
                } else {
                    equal
                };
                write(
                    registers,
                    register(instruction, code, 0)?,
                    RegisterState::Value(value),
                )?;
                return Some(());
            }
            let left = read_number(registers, nodes, register(instruction, code, 1)?)?;
            let right = read_number(registers, nodes, register(instruction, code, 2)?)?;
            if instruction.arith_feedback().is_int32_only()
                && value_type(nodes, left)? == NumericType::Int32
                && value_type(nodes, right)? == NumericType::Int32
            {
                match op {
                    Op::Equal => NumericNode::IntegerEqual(left, right),
                    Op::NotEqual => NumericNode::IntegerNotEqual(left, right),
                    _ => unreachable!("matched strict equality"),
                }
            } else {
                let left = widen_to_number(left, nodes, block_nodes)?;
                let right = widen_to_number(right, nodes, block_nodes)?;
                match op {
                    Op::Equal => NumericNode::Equal(left, right),
                    Op::NotEqual => NumericNode::NotEqual(left, right),
                    _ => unreachable!("matched strict equality"),
                }
            }
        }
        Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
            if !instruction.arith_feedback().is_numeric_only() {
                return None;
            }
            let left = read_number(registers, nodes, register(instruction, code, 1)?)?;
            let right = read_number(registers, nodes, register(instruction, code, 2)?)?;
            if instruction.arith_feedback().is_int32_only()
                && value_type(nodes, left)? == NumericType::Int32
                && value_type(nodes, right)? == NumericType::Int32
            {
                match op {
                    Op::LessThan => NumericNode::IntegerLessThan(left, right),
                    Op::LessEq => NumericNode::IntegerLessEqual(left, right),
                    Op::GreaterThan => NumericNode::IntegerGreaterThan(left, right),
                    Op::GreaterEq => NumericNode::IntegerGreaterEqual(left, right),
                    _ => unreachable!("matched int32 comparison"),
                }
            } else {
                let left = widen_to_number(left, nodes, block_nodes)?;
                let right = widen_to_number(right, nodes, block_nodes)?;
                match op {
                    Op::LessThan => NumericNode::LessThan(left, right),
                    Op::LessEq => NumericNode::LessEqual(left, right),
                    Op::GreaterThan => NumericNode::GreaterThan(left, right),
                    Op::GreaterEq => NumericNode::GreaterEqual(left, right),
                    _ => unreachable!("matched Float64 comparison"),
                }
            }
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
            | NumericNode::IntegerMul(..)
            | NumericNode::IntegerNeg(..)
            | NumericNode::IntegerAddImmediate(..)
            | NumericNode::IntegerSubImmediate(..)
    ) {
        push_frame_state(
            frame_states,
            NumericFramePoint::Node(value),
            function_id,
            instruction.byte_pc,
            registers,
            live_in,
        );
    }
    write(registers, destination, RegisterState::Value(value))
}

fn push_frame_state(
    frame_states: &mut Vec<NumericFrameState>,
    point: NumericFramePoint,
    function_id: u32,
    byte_pc: u32,
    registers: &[RegisterState],
    live_in: &[bool],
) {
    frame_states.push(NumericFrameState {
        point,
        function_id,
        byte_pc,
        slots: registers
            .iter()
            .copied()
            .zip(live_in.iter().copied())
            .map(|(state, live)| match (state, live) {
                (RegisterState::Value(value), true) => NumericFrameSlot::Value(value),
                (RegisterState::Unset, _) | (RegisterState::Value(_), false) => {
                    NumericFrameSlot::Undefined
                }
            })
            .collect(),
    });
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
        RegisterState::Value(value) => Some(RegisterState::Value(value)),
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
        NumericType::Int32 | NumericType::Uint32 | NumericType::Number
    )
    .then_some(value)
}

fn read_value(registers: &[RegisterState], register: u16) -> Option<NumericValue> {
    let RegisterState::Value(value) = read_state(registers, register)? else {
        return None;
    };
    Some(value)
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

fn read_int32_bits(
    registers: &[RegisterState],
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    register: u16,
) -> Option<NumericValue> {
    let RegisterState::Value(value) = read_state(registers, register)? else {
        return None;
    };
    let node = match value_type(nodes, value)? {
        NumericType::Int32 | NumericType::Uint32 => return Some(value),
        NumericType::Number => NumericNode::FloatToInt32(value),
        NumericType::Boolean => NumericNode::BooleanToInt32(value),
        NumericType::Tagged => return None,
    };
    let coerced = push(nodes, node);
    block_nodes.push(coerced);
    Some(coerced)
}

fn value_type(nodes: &[NumericNode], value: NumericValue) -> Option<NumericType> {
    nodes.get(value.0).copied().map(NumericNode::value_type)
}

fn to_boolean(
    value: NumericValue,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
) -> Option<NumericValue> {
    let node = match value_type(nodes, value)? {
        NumericType::Boolean => return Some(value),
        NumericType::Int32 | NumericType::Uint32 => NumericNode::IntegerToBoolean(value),
        NumericType::Number => NumericNode::FloatToBoolean(value),
        NumericType::Tagged => NumericNode::TaggedToBoolean(value),
    };
    let boolean = push(nodes, node);
    block_nodes.push(boolean);
    Some(boolean)
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
        NumericType::Uint32 => {
            let widened = push(nodes, NumericNode::WidenUint32(value));
            block_nodes.push(widened);
            Some(widened)
        }
        NumericType::Boolean => None,
        NumericType::Tagged => None,
    }
}

fn write(registers: &mut [RegisterState], register: u16, value: RegisterState) -> Option<()> {
    *registers.get_mut(usize::from(register))? = value;
    Some(())
}
