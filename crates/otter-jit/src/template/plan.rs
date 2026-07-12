//! Backend-neutral template operations over typed baseline lowering.
//!
//! # Contents
//! - [`TemplateOp`] — the complete machine-independent operation set every
//!   template backend consumes.
//! - [`TemplateInstr`] — one operation bound to its canonical instruction PC.
//! - [`TemplatePlan`] — the validated linear operation stream for one
//!   function.
//!
//! # Invariants
//! - Built strictly on top of the shared typed lowering pass: operand decoding,
//!   duplicate-PC detection, and branch-target verification happen exactly once
//!   before any backend opens an assembler.
//! - An opcode outside the supported subset rejects the whole compilation with
//!   [`Unsupported::Opcode`]; a plan never describes a partially compilable
//!   function.
//! - Branch targets are canonical instruction indices already proven to name
//!   instruction boundaries; back edges are classified here so every backend
//!   places its cooperative poll identically.
//! - Immediate operands carry final boxed `Value` bit patterns; backends
//!   materialize them without consulting the constant pool.
//!
//! # See also
//! - [`super::arm64`] — the first machine-code consumer of these operations.

use otter_bytecode::Op;
use otter_vm::{JitCompileSnapshot, Value};

use crate::baseline::{BaselinePlan, Unsupported, value_tag};

/// One machine-independent template operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplateOp {
    /// Store the boxed `Value` bit pattern into frame register `dst`.
    LoadImmediate { dst: u16, bits: u64 },
    /// Copy frame register `src` into frame register `dst`.
    Move { dst: u16, src: u16 },
    /// Unconditional branch to the canonical instruction PC `target`.
    Jump { target: u32, back_edge: bool },
    /// Branch to `target` when `ToBoolean(r<condition>)` matches
    /// `when_truthy`; fall through otherwise.
    Branch {
        condition: u16,
        target: u32,
        when_truthy: bool,
        back_edge: bool,
    },
    /// `r<dst> = ToBoolean(r<src>)`, inverted when `negate` is set.
    Truthiness { dst: u16, src: u16, negate: bool },
    /// Return `r<src>` as the completion value.
    Return { src: u16 },
    /// Return `undefined` as the completion value.
    ReturnUndefined,
}

/// One template operation at its canonical instruction PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TemplateInstr {
    pub(crate) pc: u32,
    pub(crate) op: TemplateOp,
}

/// Backend-neutral facts established before template emission starts.
pub(crate) struct TemplatePlan {
    pub(crate) instructions: Vec<TemplateInstr>,
    pub(crate) register_count: u16,
}

impl TemplatePlan {
    pub(crate) fn build(view: &JitCompileSnapshot) -> Result<Self, Unsupported> {
        let lowering = BaselinePlan::build(view)?;
        let mut instructions = Vec::with_capacity(lowering.instructions.len());
        for (meta, lowered) in view.instructions.iter().zip(&lowering.instructions) {
            let pc = lowered.instruction_pc;
            let op = match lowered.op {
                Op::LoadInt32 => {
                    let operands = lowered.load_int32_operands()?;
                    TemplateOp::LoadImmediate {
                        dst: operands.dst,
                        bits: value_tag::box_int32(operands.value),
                    }
                }
                Op::LoadNumber => {
                    let dst = lowered.destination_operands()?.dst;
                    let value = meta
                        .load_number
                        .ok_or(Unsupported::OperandShape("load-number constant"))?;
                    TemplateOp::LoadImmediate {
                        dst,
                        bits: Value::number_f64(value).to_bits(),
                    }
                }
                Op::LoadUndefined => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_UNDEFINED,
                },
                Op::LoadNull => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_NULL,
                },
                Op::LoadTrue => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_TRUE,
                },
                Op::LoadFalse => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_FALSE,
                },
                Op::LoadHole => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_HOLE,
                },
                Op::LoadLocal => {
                    let operands = lowered.local_operands()?;
                    TemplateOp::Move {
                        dst: operands.value,
                        src: operands.local,
                    }
                }
                Op::StoreLocal => {
                    let operands = lowered.local_operands()?;
                    TemplateOp::Move {
                        dst: operands.local,
                        src: operands.value,
                    }
                }
                Op::Jump => {
                    let target = lowered.branch_operands()?.target;
                    TemplateOp::Jump {
                        target,
                        back_edge: target <= pc,
                    }
                }
                Op::JumpIfTrue | Op::JumpIfFalse => {
                    let operands = lowered.conditional_branch_operands()?;
                    TemplateOp::Branch {
                        condition: operands.condition,
                        target: operands.target,
                        when_truthy: lowered.op == Op::JumpIfTrue,
                        back_edge: operands.target <= pc,
                    }
                }
                Op::ToBoolean => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::Truthiness {
                        dst: operands.dst,
                        src: operands.src,
                        negate: false,
                    }
                }
                Op::LogicalNot => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::Truthiness {
                        dst: operands.dst,
                        src: operands.src,
                        negate: true,
                    }
                }
                Op::Return | Op::ReturnValue => TemplateOp::Return {
                    src: lowered.source_operands()?.src,
                },
                Op::ReturnUndefined => TemplateOp::ReturnUndefined,
                op => return Err(Unsupported::Opcode(op)),
            };
            instructions.push(TemplateInstr { pc, op });
        }
        Ok(Self {
            instructions,
            register_count: view.code_block.register_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::Operand;
    use otter_vm::jit::JitTestInstruction;

    const STRIDE: u32 = 4;

    fn view(instrs: &[(Op, Vec<Operand>)]) -> JitCompileSnapshot {
        let instructions = instrs
            .iter()
            .enumerate()
            .map(|(idx, (op, operands))| {
                JitTestInstruction::new(*op, idx as u32, idx as u32 * STRIDE, operands.clone())
            })
            .collect();
        JitCompileSnapshot::without_feedback(0, 1, 8, instructions)
    }

    #[test]
    fn plan_maps_the_supported_subset() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(0), Operand::Imm32(-7)],
            ),
            (Op::LoadTrue, vec![Operand::Register(1)]),
            (
                Op::LogicalNot,
                vec![Operand::Register(2), Operand::Register(1)],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(1), Operand::Register(2)],
            ),
            (
                Op::StoreLocal,
                vec![Operand::Register(0), Operand::Imm32(3)],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        let plan = TemplatePlan::build(&v).expect("plan");
        assert_eq!(plan.register_count, 8);
        assert_eq!(
            plan.instructions[0].op,
            TemplateOp::LoadImmediate {
                dst: 0,
                bits: value_tag::box_int32(-7),
            }
        );
        assert_eq!(
            plan.instructions[1].op,
            TemplateOp::LoadImmediate {
                dst: 1,
                bits: value_tag::VALUE_TRUE,
            }
        );
        assert_eq!(
            plan.instructions[2].op,
            TemplateOp::Truthiness {
                dst: 2,
                src: 1,
                negate: true,
            }
        );
        assert_eq!(
            plan.instructions[3].op,
            TemplateOp::Branch {
                condition: 2,
                target: 5,
                when_truthy: false,
                back_edge: false,
            }
        );
        assert_eq!(plan.instructions[4].op, TemplateOp::Move { dst: 3, src: 0 });
        assert_eq!(plan.instructions[5].op, TemplateOp::Return { src: 0 });
    }

    #[test]
    fn plan_classifies_back_edges() {
        let v = view(&[
            (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
            (Op::Jump, vec![Operand::Imm32(-2)]),
            (Op::ReturnUndefined, vec![]),
        ]);
        let plan = TemplatePlan::build(&v).expect("plan");
        assert_eq!(
            plan.instructions[1].op,
            TemplateOp::Jump {
                target: 0,
                back_edge: true,
            }
        );
        assert_eq!(plan.instructions[2].op, TemplateOp::ReturnUndefined);
    }

    #[test]
    fn plan_rejects_the_whole_function_on_an_unsupported_opcode() {
        let v = view(&[
            (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
            (
                Op::Add,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::Register(0),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        assert_eq!(
            TemplatePlan::build(&v).err(),
            Some(Unsupported::Opcode(Op::Add))
        );
    }

    #[test]
    fn plan_rejects_non_boundary_branch_targets() {
        let v = view(&[
            (Op::Jump, vec![Operand::Imm32(8)]),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert_eq!(
            TemplatePlan::build(&v).err(),
            Some(Unsupported::BranchTarget(9))
        );
    }
}
