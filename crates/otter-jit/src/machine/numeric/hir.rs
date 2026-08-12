//! Typed scalar HIR and control-flow construction.
//!
//! # Contents
//! - [`NumericFunction`] — bounded numeric SSA graph with explicit blocks.
//! - [`NumericBlock`] and [`NumericTerminator`] — predecessor/successor edges,
//!   block parameters, edge arguments, branches, and returns.
//! - [`NumericNode`] — tagged/scalar parameters, constants, captured-binding
//!   reads, guarded coercions, ordinary properties, indexed elements,
//!   arithmetic, comparison, and typed plain/method calls.
//!
//! # Invariants
//! - Parameters remain tagged unless their uses prove a numeric representation;
//!   inferred Number/Int32 parameters are guarded before effects.
//! - Tagged values produced inside the function may enter numeric-only regions
//!   through an exact pre-operation guarded decode. A failed decode resumes the
//!   original bytecode before any observable effect can be replayed.
//! - Parameters outside the exact entry live-in set have no HIR value, load,
//!   guard, or allocator interval.
//! - Indexed loads and stores require a baked VM element program plus the GC
//!   cage. Their frame state describes the exact pre-access register window;
//!   every side exit precedes the load or the effect-only store.
//! - Ordinary property nodes exist independently of settled shape/slot
//!   metadata. Selection either emits a guarded hit or exact-deoptimizes at the
//!   original bytecode. Named `.length` loads retain their exotic fast-path
//!   marker; every property frame state describes the exact pre-access register
//!   window.
//! - Captured-binding reads require the GC cage and retain an exact pre-load
//!   frame state so an invalid spine or TDZ hole resumes canonically.
//! - A protected instruction's deopt state retains values used only by its
//!   innermost catch. This implicit liveness is solved with normal CFG
//!   liveness; element and scalar guards do not become generated throw edges.
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
use otter_vm::{JitCompileSnapshot, JitElementBase, JitInstructionMetadata};

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
    TaggedToNumber(NumericValue),
    TaggedToInt32(NumericValue),
    This,
    ClassSuperConstructor(NumericValue),
    Upvalue {
        index: i32,
        byte_pc: u32,
    },
    BindThis {
        source: NumericValue,
        logical_pc: u32,
        byte_pc: u32,
        exceptional_edge: Option<u16>,
    },
    ConstructorFieldStore {
        object: NumericValue,
        value: NumericValue,
        byte_pc: u32,
    },
    PropertyLoad {
        receiver: NumericValue,
        byte_pc: u32,
        exotic_length: bool,
    },
    PropertyStore {
        receiver: NumericValue,
        value: NumericValue,
        byte_pc: u32,
    },
    ElementLoad {
        receiver: NumericValue,
        index: NumericValue,
        byte_pc: u32,
    },
    ElementStore {
        receiver: NumericValue,
        index: NumericValue,
        value: NumericValue,
        byte_pc: u32,
    },
    TaggedToBoolean(NumericValue),
    TaggedStrictEqual(NumericValue, NumericValue),
    TaggedStringConcat(NumericValue, NumericValue),
    DirectCall {
        source: NumericValue,
        target: u16,
        arguments: NumericDirectCallArguments,
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum NumericDirectCallArguments {
    Fixed { start: u16, count: u8 },
    Spread(NumericValue),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NumericDirectCallKind {
    Plain,
    Method(otter_vm::jit::JitMethodGuard),
    Construct,
    DerivedConstruct,
    SuperConstruct,
    DerivedSuperConstruct,
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
            | Self::ClassSuperConstructor(..)
            | Self::Upvalue { .. }
            | Self::BindThis { .. }
            | Self::ConstructorFieldStore { .. }
            | Self::PropertyLoad { .. }
            | Self::PropertyStore { .. }
            | Self::ElementLoad { .. }
            | Self::ElementStore { .. }
            | Self::TaggedStringConcat(..)
            | Self::DirectCall { .. }
            | Self::BlockParameter(NumericType::Tagged) => NumericType::Tagged,
            Self::IntegerConstant(..)
            | Self::TaggedToInt32(..)
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
            | Self::TaggedToNumber(..)
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

#[derive(Debug, Clone, Copy)]
struct InstructionExceptionHandler {
    block: usize,
    exception_register: u16,
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
        let exception_handlers = build_instruction_exception_handlers(view, &raw_blocks)?;
        let live_in = build_liveness(view, &raw_blocks, &exception_handlers, register_count)?;
        let instruction_live_in = build_instruction_liveness(
            view,
            &raw_blocks,
            &live_in,
            &exception_handlers,
            register_count,
        )?;
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
                    view.derived_constructor,
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
                    &view.constructor_field_transitions,
                    &view.element_accesses,
                    view.cage_base != 0,
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
        | Op::LoadThis
        | Op::LoadUpvalue => {
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::GetPrototype => {
            let _ = read(register(instruction, code, 1)?)?;
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::BindThisValue => {
            let _ = read(register(instruction, code, 0)?)?;
        }
        Op::LoadProperty => {
            let _ = read(register(instruction, code, 1)?)?;
            let _ = instruction.const_index(code, 2)?;
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::StoreProperty => {
            let _ = read(register(instruction, code, 0)?)?;
            let _ = instruction.const_index(code, 1)?;
            let _ = read(register(instruction, code, 2)?)?;
            *origins.get_mut(usize::from(register(instruction, code, 3)?))? = 0;
        }
        Op::LoadElement => {
            let _ = read(register(instruction, code, 1)?)?;
            let _ = read(register(instruction, code, 2)?)?;
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::StoreElement => {
            let _ = read(register(instruction, code, 0)?)?;
            let _ = read(register(instruction, code, 1)?)?;
            let _ = read(register(instruction, code, 2)?)?;
        }
        Op::Call | Op::New | Op::SuperConstruct => {
            let count = usize::try_from(instruction.const_index(code, 2)?).ok()?;
            for index in 0..count {
                let _ = read(register(instruction, code, 3 + index)?)?;
            }
            let _ = read(register(instruction, code, 1)?)?;
            *origins.get_mut(usize::from(register(instruction, code, 0)?))? = 0;
        }
        Op::CallSpread | Op::NewSpread | Op::SuperConstructSpread => {
            let _ = read(register(instruction, code, 1)?)?;
            let _ = read(register(instruction, code, 2)?)?;
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
            Op::Call
                | Op::CallMethodValue
                | Op::CallSpread
                | Op::New
                | Op::NewSpread
                | Op::SuperConstruct
                | Op::SuperConstructSpread
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
        let exceptional = matches!(
            op,
            Op::Call
                | Op::CallMethodValue
                | Op::CallSpread
                | Op::New
                | Op::NewSpread
                | Op::SuperConstruct
                | Op::SuperConstructSpread
        )
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
    exception_handlers: &[Option<InstructionExceptionHandler>],
    register_count: u16,
) -> Option<Vec<Vec<bool>>> {
    let code = view.code_block.as_ref();
    let width = usize::from(register_count);
    let mut live_in = vec![vec![false; width]; blocks.len()];
    loop {
        let mut changed = false;
        for block_index in (0..blocks.len()).rev() {
            let block = blocks.get(block_index)?;
            let mut next = block_live_out(block, &live_in, width)?;
            for pc in (block.start..block.end).rev() {
                transfer_instruction_liveness(
                    view.instructions.get(pc)?,
                    code,
                    exception_handlers.get(pc).copied().flatten(),
                    &live_in,
                    &mut next,
                )?;
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
    exception_handlers: &[Option<InstructionExceptionHandler>],
    register_count: u16,
) -> Option<Vec<Vec<bool>>> {
    let code = view.code_block.as_ref();
    let width = usize::from(register_count);
    let mut instruction_live_in = vec![vec![false; width]; view.instructions.len()];
    for block in blocks {
        let mut live = block_live_out(block, block_live_in, width)?;
        for pc in (block.start..block.end).rev() {
            transfer_instruction_liveness(
                view.instructions.get(pc)?,
                code,
                exception_handlers.get(pc).copied().flatten(),
                block_live_in,
                &mut live,
            )?;
            instruction_live_in[pc] = live.clone();
        }
    }
    Some(instruction_live_in)
}

fn build_instruction_exception_handlers(
    view: &JitCompileSnapshot,
    blocks: &[RawBlock],
) -> Option<Vec<Option<InstructionExceptionHandler>>> {
    let code = view.code_block.as_ref();
    let blocks_by_pc = blocks
        .iter()
        .enumerate()
        .map(|(block, raw)| Some((u32::try_from(raw.start).ok()?, block)))
        .collect::<Option<BTreeMap<_, _>>>()?;
    let mut handlers = vec![None; view.instructions.len()];
    for (pc, handler) in handlers.iter_mut().enumerate() {
        let pc = u32::try_from(pc).ok()?;
        let Some(region) = code.control_flow().enclosing_exception_region(pc) else {
            continue;
        };
        let Some(catch_pc) = region.catch_pc else {
            continue;
        };
        *handler = Some(InstructionExceptionHandler {
            block: *blocks_by_pc.get(&catch_pc)?,
            exception_register: region.exception_register,
        });
    }
    Some(handlers)
}

fn block_live_out(
    block: &RawBlock,
    block_live_in: &[Vec<bool>],
    width: usize,
) -> Option<Vec<bool>> {
    let mut live = vec![false; width];
    for (edge, &successor) in block.successors.iter().enumerate() {
        for (register, &successor_live) in
            block_live_in.get(successor)?.iter().enumerate().take(width)
        {
            if block.exceptional_edge == Some(edge)
                && block.exception_register == u16::try_from(register).ok()
            {
                continue;
            }
            live[register] |= successor_live;
        }
    }
    Some(live)
}

fn transfer_instruction_liveness(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    exception_handler: Option<InstructionExceptionHandler>,
    block_live_in: &[Vec<bool>],
    live: &mut [bool],
) -> Option<()> {
    let (reads, writes) = instruction_accesses(instruction, code)?;
    for write in writes {
        *live.get_mut(usize::from(write))? = false;
    }
    for read in reads {
        *live.get_mut(usize::from(read))? = true;
    }
    if instruction_has_implicit_exception_side_exit(instruction.op(code))
        && let Some(handler) = exception_handler
    {
        for (register, &handler_live) in block_live_in.get(handler.block)?.iter().enumerate() {
            if register != usize::from(handler.exception_register) && handler_live {
                *live.get_mut(register)? = true;
            }
        }
    }
    Some(())
}

fn instruction_has_implicit_exception_side_exit(op: Op) -> bool {
    matches!(
        op,
        Op::GetPrototype
            | Op::LoadUpvalue
            | Op::BindThisValue
            | Op::LoadProperty
            | Op::StoreProperty
            | Op::LoadElement
            | Op::StoreElement
            | Op::ToPrimitive
            | Op::ToNumeric
            | Op::ToNumber
            | Op::ToBoolean
            | Op::LogicalNot
            | Op::Neg
            | Op::Increment
            | Op::BitwiseNot
            | Op::Add
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
            | Op::GreaterEq
            | Op::AddImm
            | Op::SubImm
            | Op::BitwiseAndImm
            | Op::LessThanImm
            | Op::EqualImm
            | Op::NotEqualImm
            | Op::JumpIfTrue
            | Op::JumpIfFalse
    )
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
        | Op::LoadThis
        | Op::LoadUpvalue => Some((Vec::new(), vec![register(instruction, code, 0)?])),
        Op::GetPrototype => Some((
            vec![register(instruction, code, 1)?],
            vec![register(instruction, code, 0)?],
        )),
        Op::BindThisValue => Some((vec![register(instruction, code, 0)?], Vec::new())),
        Op::LoadProperty => {
            let _ = instruction.const_index(code, 2)?;
            Some((
                vec![register(instruction, code, 1)?],
                vec![register(instruction, code, 0)?],
            ))
        }
        Op::StoreProperty => {
            let _ = instruction.const_index(code, 1)?;
            Some((
                vec![
                    register(instruction, code, 0)?,
                    register(instruction, code, 2)?,
                ],
                vec![register(instruction, code, 3)?],
            ))
        }
        Op::LoadElement => Some((
            vec![
                register(instruction, code, 1)?,
                register(instruction, code, 2)?,
            ],
            vec![register(instruction, code, 0)?],
        )),
        Op::StoreElement => Some((
            vec![
                register(instruction, code, 0)?,
                register(instruction, code, 1)?,
                register(instruction, code, 2)?,
            ],
            Vec::new(),
        )),
        Op::Call | Op::New | Op::SuperConstruct => {
            let count = usize::try_from(instruction.const_index(code, 2)?).ok()?;
            let mut reads = Vec::with_capacity(count + 1);
            reads.push(register(instruction, code, 1)?);
            for index in 0..count {
                reads.push(register(instruction, code, 3 + index)?);
            }
            Some((reads, vec![register(instruction, code, 0)?]))
        }
        Op::CallSpread | Op::NewSpread | Op::SuperConstructSpread => Some((
            vec![
                register(instruction, code, 1)?,
                register(instruction, code, 2)?,
            ],
            vec![register(instruction, code, 0)?],
        )),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaggedNumericDecode {
    Number,
    Int32,
}

#[derive(Clone, Copy)]
struct NumericDecodeSite<'a> {
    registers: &'a [RegisterState],
    live_in: &'a [bool],
    function_id: u32,
    byte_pc: u32,
}

fn lower_instruction(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    derived_constructor: bool,
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
    constructor_field_transitions: &rustc_hash::FxHashMap<
        u32,
        otter_vm::jit::JitConstructorFieldTransition,
    >,
    element_accesses: &rustc_hash::FxHashMap<u32, otter_vm::JitElementAccess>,
    cage_available: bool,
    direct_call_targets: &mut Vec<NumericDirectCallTarget>,
    direct_call_arguments: &mut Vec<NumericValue>,
    exceptional_edge: Option<usize>,
) -> Option<()> {
    let op = instruction.op(code);
    let decode_site = NumericDecodeSite {
        registers,
        live_in,
        function_id,
        byte_pc: instruction.byte_pc,
    };
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
        Op::LoadUpvalue => {
            if !cage_available {
                return None;
            }
            let index = instruction.imm32(code, 1)?;
            if !(0..=4095).contains(&index) {
                return None;
            }
            let value = push(
                nodes,
                NumericNode::Upvalue {
                    index,
                    byte_pc: instruction.byte_pc,
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
        Op::GetPrototype if derived_constructor => {
            let value = push(
                nodes,
                NumericNode::ClassSuperConstructor(read_value(
                    registers,
                    register(instruction, code, 1)?,
                )?),
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
        Op::BindThisValue => {
            let source = read_value(registers, register(instruction, code, 0)?)?;
            let value = push(
                nodes,
                NumericNode::BindThis {
                    source,
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
            return Some(());
        }
        Op::StoreProperty if constructor_field_transitions.contains_key(&instruction.byte_pc) => {
            let _ = instruction.const_index(code, 1)?;
            let value = push(
                nodes,
                NumericNode::ConstructorFieldStore {
                    object: read_value(registers, register(instruction, code, 0)?)?,
                    value: read_value(registers, register(instruction, code, 2)?)?,
                    byte_pc: instruction.byte_pc,
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
                register(instruction, code, 3)?,
                RegisterState::Unset,
            )?;
            return Some(());
        }
        Op::LoadProperty => {
            let _ = instruction.const_index(code, 2)?;
            let value = push(
                nodes,
                NumericNode::PropertyLoad {
                    receiver: read_value(registers, register(instruction, code, 1)?)?,
                    byte_pc: instruction.byte_pc,
                    exotic_length: instruction.load_array_length,
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
        Op::StoreProperty => {
            let _ = instruction.const_index(code, 1)?;
            let value = push(
                nodes,
                NumericNode::PropertyStore {
                    receiver: read_value(registers, register(instruction, code, 0)?)?,
                    value: read_value(registers, register(instruction, code, 2)?)?,
                    byte_pc: instruction.byte_pc,
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
                register(instruction, code, 3)?,
                RegisterState::Unset,
            )?;
            return Some(());
        }
        Op::LoadElement => {
            if !element_access_is_usable(element_accesses, instruction.byte_pc, cage_available) {
                return None;
            }
            let receiver = read_value(registers, register(instruction, code, 1)?)?;
            if value_type(nodes, receiver)? != NumericType::Tagged {
                return None;
            }
            let index = read_value(registers, register(instruction, code, 2)?)?;
            if !matches!(
                value_type(nodes, index)?,
                NumericType::Tagged | NumericType::Int32 | NumericType::Uint32
            ) {
                return None;
            }
            let value = push(
                nodes,
                NumericNode::ElementLoad {
                    receiver,
                    index,
                    byte_pc: instruction.byte_pc,
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
        Op::StoreElement => {
            if !element_access_is_usable(element_accesses, instruction.byte_pc, cage_available) {
                return None;
            }
            let receiver = read_value(registers, register(instruction, code, 0)?)?;
            if value_type(nodes, receiver)? != NumericType::Tagged {
                return None;
            }
            let index = read_value(registers, register(instruction, code, 1)?)?;
            if !matches!(
                value_type(nodes, index)?,
                NumericType::Tagged | NumericType::Int32 | NumericType::Uint32
            ) {
                return None;
            }
            let value = push(
                nodes,
                NumericNode::ElementStore {
                    receiver,
                    index,
                    value: read_value(registers, register(instruction, code, 2)?)?,
                    byte_pc: instruction.byte_pc,
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
            return Some(());
        }
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
                    arguments: NumericDirectCallArguments::Fixed {
                        start: argument_start,
                        count: u8::try_from(argument_count).ok()?,
                    },
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
        Op::New | Op::SuperConstruct => {
            let callee = *direct_constructs.get(&instruction.byte_pc)?;
            let target = NumericDirectCallTarget {
                kind: match (op, callee.plan.is_derived_constructor) {
                    (Op::New, false) => NumericDirectCallKind::Construct,
                    (Op::New, true) => NumericDirectCallKind::DerivedConstruct,
                    (Op::SuperConstruct, false) => NumericDirectCallKind::SuperConstruct,
                    (Op::SuperConstruct, true) => NumericDirectCallKind::DerivedSuperConstruct,
                    _ => return None,
                },
                callee,
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
                    arguments: NumericDirectCallArguments::Fixed {
                        start: argument_start,
                        count: u8::try_from(argument_count).ok()?,
                    },
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
        Op::CallSpread | Op::NewSpread | Op::SuperConstructSpread => {
            let callee = match op {
                Op::CallSpread => *direct_callees.get(&instruction.byte_pc)?,
                Op::NewSpread | Op::SuperConstructSpread => {
                    *direct_constructs.get(&instruction.byte_pc)?
                }
                _ => return None,
            };
            let target = NumericDirectCallTarget {
                kind: match (op, callee.plan.is_derived_constructor) {
                    (Op::CallSpread, _) => NumericDirectCallKind::Plain,
                    (Op::NewSpread, false) => NumericDirectCallKind::Construct,
                    (Op::NewSpread, true) => NumericDirectCallKind::DerivedConstruct,
                    (Op::SuperConstructSpread, false) => NumericDirectCallKind::SuperConstruct,
                    (Op::SuperConstructSpread, true) => {
                        NumericDirectCallKind::DerivedSuperConstruct
                    }
                    _ => return None,
                },
                callee,
            };
            let source = read_value(registers, register(instruction, code, 1)?)?;
            let arguments = NumericDirectCallArguments::Spread(read_value(
                registers,
                register(instruction, code, 2)?,
            )?);
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
                    arguments,
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
                    arguments: NumericDirectCallArguments::Fixed {
                        start: argument_start,
                        count: u8::try_from(argument_count).ok()?,
                    },
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
            let value = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                TaggedNumericDecode::Number,
            )?;
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::ToNumeric | Op::ToNumber => {
            let value = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                TaggedNumericDecode::Number,
            )?;
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
            let tagged_decode = if matches!(op, Op::Add | Op::Sub | Op::Mul)
                && instruction.arith_feedback().is_int32_only()
            {
                TaggedNumericDecode::Int32
            } else {
                TaggedNumericDecode::Number
            };
            let left = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                tagged_decode,
            )?;
            let right = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 2)?,
                tagged_decode,
            )?;
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
            let source = read_int32(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
            )?;
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
                decode_site,
                nodes,
                block_nodes,
                frame_states,
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
            let source = read_int32(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
            )?;
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
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
            )?;
            let right = read_int32_bits(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
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
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
            )?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            NumericNode::IntegerNot(source)
        }
        Op::Neg => {
            if !instruction.arith_feedback().is_numeric_only() {
                return None;
            }
            let tagged_decode = if instruction.arith_feedback().is_int32_only() {
                TaggedNumericDecode::Int32
            } else {
                TaggedNumericDecode::Number
            };
            let source = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                tagged_decode,
            )?;
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
            let tagged_decode = if instruction.arith_feedback().is_int32_only() {
                TaggedNumericDecode::Int32
            } else {
                TaggedNumericDecode::Number
            };
            let left = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                tagged_decode,
            )?;
            let right = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 2)?,
                tagged_decode,
            )?;
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
            let tagged_decode = if instruction.arith_feedback().is_int32_only() {
                TaggedNumericDecode::Int32
            } else {
                TaggedNumericDecode::Number
            };
            let left = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                tagged_decode,
            )?;
            let right = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 2)?,
                tagged_decode,
            )?;
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

fn element_access_is_usable(
    element_accesses: &rustc_hash::FxHashMap<u32, otter_vm::JitElementAccess>,
    byte_pc: u32,
    cage_available: bool,
) -> bool {
    cage_available
        && element_accesses.get(&byte_pc).is_some_and(|access| {
            access.type_tag != 0 && !matches!(access.base, JitElementBase::None)
        })
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
    site: NumericDecodeSite<'_>,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    frame_states: &mut Vec<NumericFrameState>,
    register: u16,
    tagged_decode: TaggedNumericDecode,
) -> Option<NumericValue> {
    let value = read_value(site.registers, register)?;
    match value_type(nodes, value)? {
        NumericType::Int32 | NumericType::Uint32 | NumericType::Number => Some(value),
        NumericType::Tagged => Some(push_tagged_numeric_decode(
            site,
            nodes,
            block_nodes,
            frame_states,
            value,
            tagged_decode,
        )),
        NumericType::Boolean => None,
    }
}

fn read_value(registers: &[RegisterState], register: u16) -> Option<NumericValue> {
    let RegisterState::Value(value) = read_state(registers, register)? else {
        return None;
    };
    Some(value)
}

fn read_int32(
    site: NumericDecodeSite<'_>,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    frame_states: &mut Vec<NumericFrameState>,
    register: u16,
) -> Option<NumericValue> {
    let value = read_value(site.registers, register)?;
    match value_type(nodes, value)? {
        NumericType::Int32 => Some(value),
        NumericType::Tagged => Some(push_tagged_numeric_decode(
            site,
            nodes,
            block_nodes,
            frame_states,
            value,
            TaggedNumericDecode::Int32,
        )),
        NumericType::Uint32 | NumericType::Number | NumericType::Boolean => None,
    }
}

fn read_int32_bits(
    site: NumericDecodeSite<'_>,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    frame_states: &mut Vec<NumericFrameState>,
    register: u16,
) -> Option<NumericValue> {
    let value = read_value(site.registers, register)?;
    let node = match value_type(nodes, value)? {
        NumericType::Int32 | NumericType::Uint32 => return Some(value),
        NumericType::Number => NumericNode::FloatToInt32(value),
        NumericType::Boolean => NumericNode::BooleanToInt32(value),
        NumericType::Tagged => {
            let number = push_tagged_numeric_decode(
                site,
                nodes,
                block_nodes,
                frame_states,
                value,
                TaggedNumericDecode::Number,
            );
            NumericNode::FloatToInt32(number)
        }
    };
    let coerced = push(nodes, node);
    block_nodes.push(coerced);
    Some(coerced)
}

fn push_tagged_numeric_decode(
    site: NumericDecodeSite<'_>,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    frame_states: &mut Vec<NumericFrameState>,
    source: NumericValue,
    decode: TaggedNumericDecode,
) -> NumericValue {
    let node = match decode {
        TaggedNumericDecode::Number => NumericNode::TaggedToNumber(source),
        TaggedNumericDecode::Int32 => NumericNode::TaggedToInt32(source),
    };
    let value = push(nodes, node);
    block_nodes.push(value);
    push_frame_state(
        frame_states,
        NumericFramePoint::Node(value),
        site.function_id,
        site.byte_pc,
        site.registers,
        site.live_in,
    );
    value
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

#[cfg(test)]
mod tests {
    use otter_bytecode::{NO_HANDLER_OFFSET, Op, Operand};
    use otter_vm::{
        JitCompileSnapshot, JitDirectCallThisMode, JitDirectCallee, JitElementAccess,
        jit::{JitDirectCallPlan, JitTestInstruction},
        jit_feedback::{ARITH_INT32, ArithFeedback},
        native_abi::NativeFrameKind,
    };

    use super::*;

    fn catch_liveness_view() -> JitCompileSnapshot {
        let instructions = vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(10),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(7),
                ],
            ),
            (Op::LoadInt32, vec![Operand::Register(5), Operand::Imm32(7)]),
            (
                Op::Call,
                vec![
                    Operand::Register(6),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(5), Operand::Imm32(41)],
            ),
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(8), Operand::Imm32(2)]),
            (
                Op::LoadElement,
                vec![
                    Operand::Register(3),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::Mul,
                vec![
                    Operand::Register(4),
                    Operand::Register(3),
                    Operand::Register(8),
                ],
            ),
            (Op::LeaveTry, Vec::new()),
            (Op::ReturnValue, vec![Operand::Register(4)]),
            (Op::ReturnValue, vec![Operand::Register(5)]),
        ];
        let mut view = JitCompileSnapshot::without_feedback(
            91,
            2,
            9,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * 8, operands)
                })
                .collect(),
        );
        view.seed_arith_feedback_for_test(8, ArithFeedback::from_bits(ARITH_INT32));
        let call_byte_pc = view.instructions[2].byte_pc;
        view.direct_callees.insert(
            call_byte_pc,
            JitDirectCallee {
                plan: JitDirectCallPlan {
                    function_id: 92,
                    code_object_id: 1,
                    entry_cell: 1,
                    tier: NativeFrameKind::Baseline,
                    this_mode: JitDirectCallThisMode::StrictOrLexical,
                    is_derived_constructor: false,
                    generated_stack_frame_bytes: Some(0),
                    param_count: 0,
                    register_count: 1,
                    own_upvalue_count: 0,
                    inherited_upvalue_count: 0,
                },
                receiver_allocation: None,
            },
        );
        view.cage_base = 0x1000;
        view.element_accesses.insert(
            view.instructions[7].byte_pc,
            JitElementAccess {
                type_tag: 1,
                base: JitElementBase::InBody { byte: 8 },
                ..JitElementAccess::default()
            },
        );
        view
    }

    fn property_view() -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(
            101,
            0,
            4,
            vec![
                JitTestInstruction::new(
                    Op::LoadInt32,
                    0,
                    0,
                    vec![Operand::Register(0), Operand::Imm32(7)],
                ),
                JitTestInstruction::new(
                    Op::LoadProperty,
                    1,
                    8,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::ConstIndex(9),
                    ],
                ),
                JitTestInstruction::new(Op::LoadTrue, 2, 16, vec![Operand::Register(1)]),
                JitTestInstruction::new(
                    Op::StoreProperty,
                    3,
                    24,
                    vec![
                        Operand::Register(0),
                        Operand::ConstIndex(10),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 4, 32, vec![Operand::Register(2)]),
            ],
        )
    }

    fn property_catch_liveness_view() -> JitCompileSnapshot {
        let instructions = vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(10),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(7),
                ],
            ),
            (Op::LoadInt32, vec![Operand::Register(5), Operand::Imm32(7)]),
            (
                Op::Call,
                vec![
                    Operand::Register(6),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(5), Operand::Imm32(41)],
            ),
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::Nop, Vec::new()),
            (Op::Nop, Vec::new()),
            (
                Op::LoadProperty,
                vec![
                    Operand::Register(3),
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                ],
            ),
            (Op::Nop, Vec::new()),
            (Op::LeaveTry, Vec::new()),
            (Op::ReturnValue, vec![Operand::Register(3)]),
            (Op::ReturnValue, vec![Operand::Register(5)]),
        ];
        let mut view = JitCompileSnapshot::without_feedback(
            102,
            2,
            8,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * 8, operands)
                })
                .collect(),
        );
        let call_byte_pc = view.instructions[2].byte_pc;
        view.direct_callees.insert(
            call_byte_pc,
            JitDirectCallee {
                plan: JitDirectCallPlan {
                    function_id: 103,
                    code_object_id: 1,
                    entry_cell: 1,
                    tier: NativeFrameKind::Baseline,
                    this_mode: JitDirectCallThisMode::StrictOrLexical,
                    is_derived_constructor: false,
                    generated_stack_frame_bytes: Some(0),
                    param_count: 0,
                    register_count: 1,
                    own_upvalue_count: 0,
                    inherited_upvalue_count: 0,
                },
                receiver_allocation: None,
            },
        );
        view
    }

    #[test]
    fn ordinary_properties_build_without_settled_metadata_and_keep_exact_states() {
        let view = property_view();
        assert!(view.property_loads.is_empty());
        assert!(view.property_stores.is_empty());

        let hir = NumericFunction::build(&view).expect("property HIR without settled metadata");
        let receiver = hir
            .nodes
            .iter()
            .position(|node| *node == NumericNode::IntegerConstant(7))
            .map(NumericValue)
            .expect("Int32 receiver");
        let stored = hir
            .nodes
            .iter()
            .position(|node| *node == NumericNode::BooleanConstant(true))
            .map(NumericValue)
            .expect("Boolean stored value");
        let load = hir
            .nodes
            .iter()
            .position(|node| {
                *node
                    == NumericNode::PropertyLoad {
                        receiver,
                        byte_pc: 8,
                        exotic_length: false,
                    }
            })
            .map(NumericValue)
            .expect("ordinary property load");
        let store = hir
            .nodes
            .iter()
            .position(|node| {
                *node
                    == NumericNode::PropertyStore {
                        receiver,
                        value: stored,
                        byte_pc: 24,
                    }
            })
            .map(NumericValue)
            .expect("ordinary property store");
        assert_eq!(hir.nodes[load.0].value_type(), NumericType::Tagged);

        let load_state = hir
            .frame_states
            .iter()
            .find(|state| state.point == NumericFramePoint::Node(load))
            .expect("exact pre-load state");
        assert_eq!(load_state.byte_pc, 8);
        assert_eq!(load_state.slots[0], NumericFrameSlot::Value(receiver));
        assert_eq!(load_state.slots[2], NumericFrameSlot::Undefined);

        let store_state = hir
            .frame_states
            .iter()
            .find(|state| state.point == NumericFramePoint::Node(store))
            .expect("exact pre-store state");
        assert_eq!(store_state.byte_pc, 24);
        assert_eq!(store_state.slots[0], NumericFrameSlot::Value(receiver));
        assert_eq!(store_state.slots[1], NumericFrameSlot::Value(stored));
        assert_eq!(store_state.slots[2], NumericFrameSlot::Value(load));
        assert_eq!(store_state.slots[3], NumericFrameSlot::Undefined);
    }

    #[test]
    fn named_length_property_load_retains_its_exotic_marker() {
        let mut view = property_view();
        view.instructions[1].load_array_length = true;

        let hir = NumericFunction::build(&view).expect("named length property HIR");
        assert!(hir.nodes.iter().any(|node| matches!(
            node,
            NumericNode::PropertyLoad {
                byte_pc: 8,
                exotic_length: true,
                ..
            }
        )));
    }

    #[test]
    fn property_inference_and_accesses_validate_constants_and_register_roles() {
        let view = property_view();
        let code = view.code_block.as_ref();
        let mut origins = vec![1, 2, 4, 8];
        let mut int32_parameters = 0;
        let mut number_parameters = 0;

        infer_instruction_parameters(
            &view.instructions[1],
            code,
            &mut origins,
            &mut int32_parameters,
            &mut number_parameters,
        )
        .expect("property load inference");
        assert_eq!(origins, [1, 2, 0, 8]);
        assert_eq!(view.instructions[1].const_index(code, 2), Some(9));
        assert_eq!(
            instruction_accesses(&view.instructions[1], code),
            Some((vec![0], vec![2]))
        );

        infer_instruction_parameters(
            &view.instructions[3],
            code,
            &mut origins,
            &mut int32_parameters,
            &mut number_parameters,
        )
        .expect("property store inference");
        assert_eq!(origins, [1, 2, 0, 0]);
        assert_eq!(view.instructions[3].const_index(code, 1), Some(10));
        assert_eq!(
            instruction_accesses(&view.instructions[3], code),
            Some((vec![0, 1], vec![3]))
        );
        assert_eq!(int32_parameters, 0);
        assert_eq!(number_parameters, 0);
    }

    #[test]
    fn property_store_kills_its_accessor_scratch() {
        let view = JitCompileSnapshot::without_feedback(
            104,
            2,
            3,
            vec![
                JitTestInstruction::new(
                    Op::StoreProperty,
                    0,
                    0,
                    vec![
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(2)]),
            ],
        );
        assert!(
            NumericFunction::build(&view).is_none(),
            "the opaque setter scratch must not become a reusable HIR value"
        );
    }

    #[test]
    fn load_property_implicit_exception_exit_keeps_catch_only_values_live() {
        let view = property_catch_liveness_view();
        let raw_blocks = build_raw_blocks(&view).expect("exception-aware raw blocks");
        let handlers = build_instruction_exception_handlers(&view, &raw_blocks)
            .expect("per-instruction catch handlers");
        let live_in = build_liveness(&view, &raw_blocks, &handlers, 8)
            .expect("exception-aware block liveness");
        let property_block = raw_blocks
            .iter()
            .position(|block| block.start == 5)
            .expect("property block after normal boundary");
        assert!(
            live_in[property_block][5],
            "LoadProperty may throw to the catch that reads the redefined value"
        );

        let hir = NumericFunction::build(&view).expect("exception-aware property HIR");
        let catch_value = hir
            .nodes
            .iter()
            .position(|node| *node == NumericNode::IntegerConstant(41))
            .map(NumericValue)
            .expect("post-call catch-only definition");
        let property = hir
            .nodes
            .iter()
            .position(|node| matches!(node, NumericNode::PropertyLoad { .. }))
            .map(NumericValue)
            .expect("property load node");
        let state = hir
            .frame_states
            .iter()
            .find(|state| state.point == NumericFramePoint::Node(property))
            .expect("exact pre-property state");
        assert_eq!(state.slots[5], NumericFrameSlot::Value(catch_value));
        assert_eq!(state.slots[7], NumericFrameSlot::Undefined);
    }

    #[test]
    fn catch_only_definition_survives_later_element_and_decode_deopts() {
        let view = catch_liveness_view();
        let raw_blocks = build_raw_blocks(&view).expect("exception-aware raw blocks");
        let handlers = build_instruction_exception_handlers(&view, &raw_blocks)
            .expect("per-instruction catch handlers");
        let live_in = build_liveness(&view, &raw_blocks, &handlers, 9)
            .expect("exception-aware block liveness");
        let after_call = raw_blocks
            .iter()
            .position(|block| block.start == 3)
            .expect("post-call definition block");
        let element_block = raw_blocks
            .iter()
            .position(|block| block.start == 5)
            .expect("element block after normal boundary");
        assert!(
            !live_in[after_call][5],
            "the definition must kill the call-edge value at block entry"
        );
        assert!(
            live_in[element_block][5],
            "the later catch side exit must retain the redefined value across the boundary"
        );

        let hir = NumericFunction::build(&view).expect("exception-aware numeric HIR");
        let catch_value = hir
            .nodes
            .iter()
            .position(|node| *node == NumericNode::IntegerConstant(41))
            .map(NumericValue)
            .expect("post-call catch-only definition");
        let element = hir
            .nodes
            .iter()
            .position(|node| matches!(node, NumericNode::ElementLoad { .. }))
            .map(NumericValue)
            .expect("element load node");
        let decode = hir
            .nodes
            .iter()
            .position(|node| matches!(node, NumericNode::TaggedToInt32(_)))
            .map(NumericValue)
            .expect("tagged Int32 decode node");

        for point in [element, decode] {
            let state = hir
                .frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(point))
                .expect("exact pre-operation frame state");
            assert_eq!(
                state.slots[5],
                NumericFrameSlot::Value(catch_value),
                "catch-only redefinition must survive at {point:?}"
            );
            assert_eq!(
                state.slots[7],
                NumericFrameSlot::Undefined,
                "the handler supplies the exception register"
            );
        }
    }
}
