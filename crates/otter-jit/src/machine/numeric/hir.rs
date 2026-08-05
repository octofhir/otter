//! Typed, side-effect-free numeric HIR selection from authoritative bytecode.
//!
//! # Contents
//! - [`NumericFunction`] — bounded numeric expression graph.
//! - [`NumericNode`] — typed parameter, constant, and arithmetic nodes.
//! - Strict decoding of compiler-generated local-copy scaffolding.
//!
//! # Invariants
//! - Every accepted parameter is guarded as a JavaScript Number before effects.
//! - Accepted nodes cannot allocate, call, branch, touch the heap, or throw.
//! - Numeric coercion opcodes are erased only after their inputs are proven.
//! - The graph has one terminal numeric return; profitability belongs to tier
//!   policy, not to a benchmark-shaped operation-count threshold.

use otter_bytecode::{Op, Operand};
use otter_vm::{JitCompileSnapshot, JitInstructionMetadata};

const MAX_LEAF_INSTRUCTIONS: usize = 512;
const MAX_LEAF_PARAMETERS: u16 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NumericValue(pub(super) usize);

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum NumericNode {
    Parameter(u16),
    Constant(f64),
    Add(NumericValue, NumericValue),
    Sub(NumericValue, NumericValue),
    Mul(NumericValue, NumericValue),
    Div(NumericValue, NumericValue),
    Neg(NumericValue),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisterState {
    Unset,
    Undefined,
    Value(NumericValue),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NumericFunction {
    pub(super) nodes: Vec<NumericNode>,
    pub(super) result: NumericValue,
    pub(super) parameter_count: u16,
    pub(super) register_count: u16,
    pub(super) arithmetic_op_count: usize,
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
            || parameter_count > MAX_LEAF_PARAMETERS
            || view.instructions.is_empty()
            || view.instructions.len() > MAX_LEAF_INSTRUCTIONS
        {
            return None;
        }

        let mut nodes = Vec::with_capacity(view.instructions.len());
        let mut registers = vec![RegisterState::Unset; usize::from(register_count)];
        for parameter in 0..parameter_count {
            let value = push(&mut nodes, NumericNode::Parameter(parameter));
            registers[usize::from(parameter)] = RegisterState::Value(value);
        }

        let mut arithmetic_op_count = 0usize;
        let mut result = None;
        for (index, instruction) in view.instructions.iter().enumerate() {
            if result.is_some() {
                return None;
            }
            let op = instruction.op(code);
            match op {
                Op::StoreLocal => {
                    let source = register(instruction, code, 0)?;
                    let local = local_index(instruction, code, 1)?;
                    let value = read_state(&registers, source)?;
                    write(&mut registers, local, value)?;
                }
                Op::LoadLocal => {
                    let destination = register(instruction, code, 0)?;
                    let local = local_index(instruction, code, 1)?;
                    let value = read_state(&registers, local)?;
                    write(&mut registers, destination, value)?;
                }
                Op::LoadUndefined => {
                    write(
                        &mut registers,
                        register(instruction, code, 0)?,
                        RegisterState::Undefined,
                    )?;
                }
                Op::LoadInt32 => {
                    let destination = register(instruction, code, 0)?;
                    let value = f64::from(instruction.imm32(code, 1)?);
                    let node = push(&mut nodes, NumericNode::Constant(value));
                    write(&mut registers, destination, RegisterState::Value(node))?;
                }
                Op::LoadNumber => {
                    let destination = register(instruction, code, 0)?;
                    instruction.const_index(code, 1)?;
                    let node = push(&mut nodes, NumericNode::Constant(instruction.load_number?));
                    write(&mut registers, destination, RegisterState::Value(node))?;
                }
                Op::ToPrimitive => {
                    let destination = register(instruction, code, 0)?;
                    let source = register(instruction, code, 1)?;
                    instruction.const_index(code, 2)?;
                    let value = read_numeric(&registers, source)?;
                    write(&mut registers, destination, RegisterState::Value(value))?;
                }
                Op::ToNumeric => {
                    let destination = register(instruction, code, 0)?;
                    let source = register(instruction, code, 1)?;
                    let value = read_numeric(&registers, source)?;
                    write(&mut registers, destination, RegisterState::Value(value))?;
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div => {
                    if !instruction.arith_feedback().is_numeric_only() {
                        return None;
                    }
                    let destination = register(instruction, code, 0)?;
                    let left = read_numeric(&registers, register(instruction, code, 1)?)?;
                    let right = read_numeric(&registers, register(instruction, code, 2)?)?;
                    let node = match op {
                        Op::Add => NumericNode::Add(left, right),
                        Op::Sub => NumericNode::Sub(left, right),
                        Op::Mul => NumericNode::Mul(left, right),
                        Op::Div => NumericNode::Div(left, right),
                        _ => unreachable!("matched numeric binary operation"),
                    };
                    let node = push(&mut nodes, node);
                    write(&mut registers, destination, RegisterState::Value(node))?;
                    arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
                }
                Op::Neg => {
                    if !instruction.arith_feedback().is_numeric_only() {
                        return None;
                    }
                    let destination = register(instruction, code, 0)?;
                    let source = read_numeric(&registers, register(instruction, code, 1)?)?;
                    let node = push(&mut nodes, NumericNode::Neg(source));
                    write(&mut registers, destination, RegisterState::Value(node))?;
                    arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
                }
                Op::ReturnValue if index + 1 == view.instructions.len() => {
                    result = Some(read_numeric(&registers, register(instruction, code, 0)?)?);
                }
                _ => return None,
            }
        }

        Some(Self {
            nodes,
            result: result?,
            parameter_count,
            register_count,
            arithmetic_op_count,
        })
    }
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

fn read_numeric(registers: &[RegisterState], register: u16) -> Option<NumericValue> {
    match read_state(registers, register)? {
        RegisterState::Value(value) => Some(value),
        RegisterState::Unset | RegisterState::Undefined => None,
    }
}

fn write(registers: &mut [RegisterState], register: u16, value: RegisterState) -> Option<()> {
    *registers.get_mut(usize::from(register))? = value;
    Some(())
}
