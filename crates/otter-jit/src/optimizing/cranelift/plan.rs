//! Conservative numeric-leaf planning directly from authoritative bytecode.
//!
//! # Contents
//! - [`NumericLeafPlan`] — a linear, side-effect-free Number expression graph.
//! - [`NumericNode`] — parameter, constant, and IEEE-754 arithmetic nodes.
//! - Strict operand decoding for the source compiler's local-copy scaffolding.
//!
//! # Invariants
//! - Every accepted parameter is guarded as a JavaScript Number before the
//!   first bytecode effect; a miss can therefore resume at logical PC zero.
//! - Accepted instructions cannot allocate, call user code, touch the heap,
//!   branch, throw, or mutate state visible outside the current frame.
//! - `ToPrimitive` and `ToNumeric` are erased only after their inputs have
//!   already been proven numeric by construction.
//! - The plan is bounded and contains one terminal `ReturnValue`.
//!
//! # See also
//! - [`super::lower`] lowers this graph to Cranelift IR.
//! - `crate::optimizing::arm64` remains the general optimizing backend.

use otter_bytecode::{Op, Operand};
use otter_vm::{JitCompileSnapshot, JitInstructionMetadata};

const MIN_ARITHMETIC_OPS: usize = 8;
const MAX_LEAF_INSTRUCTIONS: usize = 512;
const MAX_LEAF_PARAMETERS: u16 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NumericNodeId(pub(super) usize);

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum NumericNode {
    Parameter(u16),
    Constant(f64),
    Add(NumericNodeId, NumericNodeId),
    Sub(NumericNodeId, NumericNodeId),
    Mul(NumericNodeId, NumericNodeId),
    Div(NumericNodeId, NumericNodeId),
    Neg(NumericNodeId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumericRegister {
    Unset,
    Undefined,
    Node(NumericNodeId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NumericSource {
    pub(super) logical_pc: u32,
    pub(super) byte_pc: u32,
    pub(super) operation: Op,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NumericLeafPlan {
    pub(super) nodes: Vec<NumericNode>,
    pub(super) node_sources: Vec<Option<NumericSource>>,
    pub(super) result: NumericNodeId,
    pub(super) return_source: NumericSource,
    pub(super) parameter_count: u16,
    pub(super) register_count: u16,
    pub(super) arithmetic_op_count: usize,
}

impl NumericLeafPlan {
    pub(super) fn build(view: &JitCompileSnapshot) -> Option<Self> {
        let code_block = view.code_block.as_ref();
        let parameter_count = code_block.param_count;
        let register_count = code_block.register_count;
        if code_block.is_async
            || code_block.is_generator
            || code_block.is_async_generator
            || parameter_count > register_count
            || parameter_count > MAX_LEAF_PARAMETERS
            || view.instructions.is_empty()
            || view.instructions.len() > MAX_LEAF_INSTRUCTIONS
        {
            return None;
        }

        let mut nodes = Vec::with_capacity(view.instructions.len());
        let mut node_sources = Vec::with_capacity(view.instructions.len());
        let mut registers = vec![NumericRegister::Unset; usize::from(register_count)];
        for parameter in 0..parameter_count {
            let id = push_node(
                &mut nodes,
                &mut node_sources,
                NumericNode::Parameter(parameter),
                None,
            );
            registers[usize::from(parameter)] = NumericRegister::Node(id);
        }

        let mut arithmetic_op_count = 0usize;
        let mut result = None;
        let mut return_source = None;
        for (index, instruction) in view.instructions.iter().enumerate() {
            if result.is_some() {
                return None;
            }
            let op = instruction.op(code_block);
            match op {
                Op::StoreLocal => {
                    let source = register(instruction, code_block, 0)?;
                    let local = local_index(instruction, code_block, 1)?;
                    let value = read_register_state(&registers, source)?;
                    write_register(&mut registers, local, value)?;
                }
                Op::LoadLocal => {
                    let destination = register(instruction, code_block, 0)?;
                    let local = local_index(instruction, code_block, 1)?;
                    let value = read_register_state(&registers, local)?;
                    write_register(&mut registers, destination, value)?;
                }
                Op::LoadUndefined => {
                    let destination = register(instruction, code_block, 0)?;
                    write_register(&mut registers, destination, NumericRegister::Undefined)?;
                }
                Op::LoadInt32 => {
                    let destination = register(instruction, code_block, 0)?;
                    let value = f64::from(instruction.imm32(code_block, 1)?);
                    let source = instruction_source(instruction, code_block, op)?;
                    let node = push_node(
                        &mut nodes,
                        &mut node_sources,
                        NumericNode::Constant(value),
                        Some(source),
                    );
                    write_register(&mut registers, destination, NumericRegister::Node(node))?;
                }
                Op::LoadNumber => {
                    let destination = register(instruction, code_block, 0)?;
                    instruction.const_index(code_block, 1)?;
                    let value = instruction.load_number?;
                    let source = instruction_source(instruction, code_block, op)?;
                    let node = push_node(
                        &mut nodes,
                        &mut node_sources,
                        NumericNode::Constant(value),
                        Some(source),
                    );
                    write_register(&mut registers, destination, NumericRegister::Node(node))?;
                }
                Op::ToPrimitive => {
                    let destination = register(instruction, code_block, 0)?;
                    let source = register(instruction, code_block, 1)?;
                    instruction.const_index(code_block, 2)?;
                    let value = read_numeric_register(&registers, source)?;
                    write_register(&mut registers, destination, NumericRegister::Node(value))?;
                }
                Op::ToNumeric => {
                    let destination = register(instruction, code_block, 0)?;
                    let source = register(instruction, code_block, 1)?;
                    let value = read_numeric_register(&registers, source)?;
                    write_register(&mut registers, destination, NumericRegister::Node(value))?;
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div => {
                    if !instruction.arith_feedback().is_numeric_only() {
                        return None;
                    }
                    let destination = register(instruction, code_block, 0)?;
                    let left =
                        read_numeric_register(&registers, register(instruction, code_block, 1)?)?;
                    let right =
                        read_numeric_register(&registers, register(instruction, code_block, 2)?)?;
                    let node = match op {
                        Op::Add => NumericNode::Add(left, right),
                        Op::Sub => NumericNode::Sub(left, right),
                        Op::Mul => NumericNode::Mul(left, right),
                        Op::Div => NumericNode::Div(left, right),
                        _ => unreachable!("matched numeric binary opcode"),
                    };
                    let source = instruction_source(instruction, code_block, op)?;
                    let node = push_node(&mut nodes, &mut node_sources, node, Some(source));
                    write_register(&mut registers, destination, NumericRegister::Node(node))?;
                    arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
                }
                Op::Neg => {
                    if !instruction.arith_feedback().is_numeric_only() {
                        return None;
                    }
                    let destination = register(instruction, code_block, 0)?;
                    let source =
                        read_numeric_register(&registers, register(instruction, code_block, 1)?)?;
                    let source_metadata = instruction_source(instruction, code_block, op)?;
                    let node = push_node(
                        &mut nodes,
                        &mut node_sources,
                        NumericNode::Neg(source),
                        Some(source_metadata),
                    );
                    write_register(&mut registers, destination, NumericRegister::Node(node))?;
                    arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
                }
                Op::ReturnValue if index + 1 == view.instructions.len() => {
                    let source = register(instruction, code_block, 0)?;
                    result = Some(read_numeric_register(&registers, source)?);
                    return_source = Some(instruction_source(instruction, code_block, op)?);
                }
                _ => return None,
            }
        }

        if arithmetic_op_count < MIN_ARITHMETIC_OPS {
            return None;
        }
        Some(Self {
            nodes,
            node_sources,
            result: result?,
            return_source: return_source?,
            parameter_count,
            register_count,
            arithmetic_op_count,
        })
    }

    pub(super) fn source_for_logical_pc(&self, logical_pc: u32) -> Option<&NumericSource> {
        self.node_sources
            .iter()
            .flatten()
            .chain(std::iter::once(&self.return_source))
            .find(|source| source.logical_pc == logical_pc)
    }
}

fn push_node(
    nodes: &mut Vec<NumericNode>,
    sources: &mut Vec<Option<NumericSource>>,
    node: NumericNode,
    source: Option<NumericSource>,
) -> NumericNodeId {
    let id = NumericNodeId(nodes.len());
    nodes.push(node);
    sources.push(source);
    id
}

fn instruction_source(
    instruction: &JitInstructionMetadata,
    code_block: &otter_vm::CodeBlock,
    op: Op,
) -> Option<NumericSource> {
    Some(NumericSource {
        logical_pc: instruction.instruction_pc(code_block),
        byte_pc: instruction.byte_pc(),
        operation: op,
    })
}

fn register(
    instruction: &JitInstructionMetadata,
    code_block: &otter_vm::CodeBlock,
    index: usize,
) -> Option<u16> {
    match instruction.operand(code_block, index) {
        Some(Operand::Register(register)) => Some(register),
        _ => None,
    }
}

fn local_index(
    instruction: &JitInstructionMetadata,
    code_block: &otter_vm::CodeBlock,
    index: usize,
) -> Option<u16> {
    u16::try_from(instruction.imm32(code_block, index)?).ok()
}

fn read_register_state(registers: &[NumericRegister], register: u16) -> Option<NumericRegister> {
    match registers.get(usize::from(register)).copied()? {
        NumericRegister::Unset => None,
        value => Some(value),
    }
}

fn read_numeric_register(registers: &[NumericRegister], register: u16) -> Option<NumericNodeId> {
    match read_register_state(registers, register)? {
        NumericRegister::Node(node) => Some(node),
        NumericRegister::Undefined | NumericRegister::Unset => None,
    }
}

fn write_register(
    registers: &mut [NumericRegister],
    register: u16,
    value: NumericRegister,
) -> Option<()> {
    *registers.get_mut(usize::from(register))? = value;
    Some(())
}

#[cfg(test)]
mod tests {
    use otter_vm::{
        jit::JitTestInstruction,
        jit_feedback::{ARITH_INT32, ArithFeedback},
    };

    use super::*;

    fn view(instructions: Vec<(Op, Vec<Operand>)>) -> JitCompileSnapshot {
        let mut view = JitCompileSnapshot::without_feedback(
            17,
            2,
            8,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * 7, operands)
                })
                .collect(),
        );
        for instruction in &mut view.instructions {
            if instruction.op(view.code_block.as_ref()) == Op::LoadNumber {
                instruction.load_number = Some(1.25);
            }
        }
        for pc in 0..view.instructions.len() {
            if matches!(
                view.instructions[pc].op(view.code_block.as_ref()),
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Neg
            ) {
                view.seed_arith_feedback_for_test(pc as u32, ArithFeedback::from_bits(ARITH_INT32));
            }
        }
        view
    }

    #[test]
    fn accepts_bounded_pure_numeric_graph() {
        let view = view(vec![
            (Op::LoadUndefined, vec![Operand::Register(5)]),
            (
                Op::StoreLocal,
                vec![Operand::Register(5), Operand::Imm32(4)],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Mul,
                vec![
                    Operand::Register(3),
                    Operand::Register(2),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Sub,
                vec![
                    Operand::Register(4),
                    Operand::Register(3),
                    Operand::Register(0),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(4),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Mul,
                vec![
                    Operand::Register(3),
                    Operand::Register(2),
                    Operand::Register(0),
                ],
            ),
            (
                Op::Sub,
                vec![
                    Operand::Register(4),
                    Operand::Register(3),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Div,
                vec![
                    Operand::Register(2),
                    Operand::Register(4),
                    Operand::Register(0),
                ],
            ),
            (Op::Neg, vec![Operand::Register(3), Operand::Register(2)]),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ]);

        let plan = NumericLeafPlan::build(&view).expect("pure numeric leaf");
        assert_eq!(plan.arithmetic_op_count, 8);
        assert_eq!(plan.parameter_count, 2);
    }

    #[test]
    fn rejects_observable_or_heap_operations() {
        let view = view(vec![
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::LoadProperty,
                vec![
                    Operand::Register(3),
                    Operand::Register(2),
                    Operand::ConstIndex(0),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ]);

        assert!(NumericLeafPlan::build(&view).is_none());
    }

    #[test]
    fn rejects_unseen_or_non_numeric_arithmetic_feedback() {
        let mut view = view(vec![
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(3),
                    Operand::Register(2),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(4),
                    Operand::Register(3),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(5),
                    Operand::Register(4),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(6),
                    Operand::Register(5),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(7),
                    Operand::Register(6),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(7),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(3),
                    Operand::Register(2),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ]);

        view.seed_arith_feedback_for_test(4, ArithFeedback::default());
        assert!(NumericLeafPlan::build(&view).is_none());

        view.seed_arith_feedback_for_test(4, ArithFeedback::from_bits(1 << 2));
        assert!(NumericLeafPlan::build(&view).is_none());
    }

    #[test]
    fn rejects_async_and_generator_execution_protocols() {
        let instructions = vec![
            (Op::Neg, vec![Operand::Register(2), Operand::Register(0)]),
            (Op::Neg, vec![Operand::Register(3), Operand::Register(2)]),
            (Op::Neg, vec![Operand::Register(4), Operand::Register(3)]),
            (Op::Neg, vec![Operand::Register(5), Operand::Register(4)]),
            (Op::Neg, vec![Operand::Register(6), Operand::Register(5)]),
            (Op::Neg, vec![Operand::Register(7), Operand::Register(6)]),
            (Op::Neg, vec![Operand::Register(2), Operand::Register(7)]),
            (Op::Neg, vec![Operand::Register(3), Operand::Register(2)]),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ];

        let mut async_view = view(instructions.clone());
        std::sync::Arc::get_mut(&mut async_view.code_block)
            .expect("test owns the CodeBlock")
            .is_async = true;
        assert!(NumericLeafPlan::build(&async_view).is_none());

        let mut generator_view = view(instructions);
        std::sync::Arc::get_mut(&mut generator_view.code_block)
            .expect("test owns the CodeBlock")
            .is_generator = true;
        assert!(NumericLeafPlan::build(&generator_view).is_none());
    }
}
