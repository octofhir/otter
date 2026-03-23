use otter_vm_bytecode::function::ArithmeticType;

use crate::feedback::IcSnapshot;
use crate::mir::graph::BlockId;
use crate::mir::nodes::{HelperKind, MirOp};

use super::context::BuilderContext;

#[derive(Clone, Copy)]
pub(super) enum BinaryArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Copy)]
pub(super) struct BinaryArithRegs {
    pub(super) dst: u16,
    pub(super) lhs: u16,
    pub(super) rhs: u16,
}

pub(super) fn lower_binary_arith(
    ctx: &mut BuilderContext<'_>,
    block: BlockId,
    pc: u32,
    regs: BinaryArithRegs,
    feedback_index: u16,
    op: BinaryArithOp,
) {
    let l = ctx.get_scratch(block, regs.lhs, pc);
    let r = ctx.get_scratch(block, regs.rhs, pc);
    let ic = ctx.feedback.ic(feedback_index);

    let result = match ic {
        IcSnapshot::Arithmetic(ArithmeticType::Int32) => {
            let deopt = ctx.make_deopt(pc);
            let gl = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let raw = match op {
                BinaryArithOp::Add => ctx.graph.push_instr(
                    block,
                    MirOp::AddI32 {
                        lhs: gl,
                        rhs: gr,
                        deopt,
                    },
                    pc,
                ),
                BinaryArithOp::Sub => ctx.graph.push_instr(
                    block,
                    MirOp::SubI32 {
                        lhs: gl,
                        rhs: gr,
                        deopt,
                    },
                    pc,
                ),
                BinaryArithOp::Mul => ctx.graph.push_instr(
                    block,
                    MirOp::MulI32 {
                        lhs: gl,
                        rhs: gr,
                        deopt,
                    },
                    pc,
                ),
                BinaryArithOp::Div => ctx.graph.push_instr(
                    block,
                    MirOp::DivI32 {
                        lhs: gl,
                        rhs: gr,
                        deopt,
                    },
                    pc,
                ),
            };
            ctx.graph.push_instr(block, MirOp::BoxInt32(raw), pc)
        }
        IcSnapshot::Arithmetic(ArithmeticType::Number) => {
            let deopt = ctx.make_deopt(pc);
            let gl = ctx
                .graph
                .push_instr(block, MirOp::GuardFloat64 { val: l, deopt }, pc);
            let gr = ctx
                .graph
                .push_instr(block, MirOp::GuardFloat64 { val: r, deopt }, pc);
            let raw = match op {
                BinaryArithOp::Add => {
                    ctx.graph
                        .push_instr(block, MirOp::AddF64 { lhs: gl, rhs: gr }, pc)
                }
                BinaryArithOp::Sub => {
                    ctx.graph
                        .push_instr(block, MirOp::SubF64 { lhs: gl, rhs: gr }, pc)
                }
                BinaryArithOp::Mul => {
                    ctx.graph
                        .push_instr(block, MirOp::MulF64 { lhs: gl, rhs: gr }, pc)
                }
                BinaryArithOp::Div => {
                    ctx.graph
                        .push_instr(block, MirOp::DivF64 { lhs: gl, rhs: gr }, pc)
                }
            };
            ctx.graph.push_instr(block, MirOp::BoxFloat64(raw), pc)
        }
        _ => {
            let kind = match op {
                BinaryArithOp::Add => HelperKind::GenericAdd,
                BinaryArithOp::Sub => HelperKind::GenericSub,
                BinaryArithOp::Mul => HelperKind::GenericMul,
                BinaryArithOp::Div => HelperKind::GenericDiv,
            };
            ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind,
                    args: vec![l, r],
                },
                pc,
            )
        }
    };

    ctx.set_scratch(block, regs.dst, result, pc);
}
