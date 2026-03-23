use otter_vm_bytecode::instruction::Instruction;

use crate::mir::graph::BlockId;
use crate::mir::nodes::{HelperKind, MirOp};

use super::arithmetic::{BinaryArithOp, BinaryArithRegs, lower_binary_arith};
use super::context::BuilderContext;

pub(super) fn lower_instruction(
    ctx: &mut BuilderContext<'_>,
    block: BlockId,
    pc: u32,
    inst: &Instruction,
) -> bool {
    match inst {
        Instruction::LoadUndefined { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::Undefined, pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::LoadNull { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::Null, pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::LoadTrue { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::True, pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::LoadFalse { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::False, pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::LoadInt8 { dst, value } => {
            let v = ctx
                .graph
                .push_instr(block, MirOp::ConstInt32(*value as i32), pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(v), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::LoadInt32 { dst, value } => {
            let v = ctx.graph.push_instr(block, MirOp::ConstInt32(*value), pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(v), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::LoadConst { dst, idx } => {
            let v = ctx.graph.push_instr(block, MirOp::LoadConstPool(idx.0), pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::GetLocal { dst, idx } => {
            let v = ctx.load_local(block, idx.0, pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::SetLocal { idx, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            ctx.store_local(block, idx.0, val, pc);
            true
        }
        Instruction::GetUpvalue { dst, idx } => {
            let v = ctx.graph.push_instr(block, MirOp::LoadUpvalue(idx.0), pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::SetUpvalue { idx, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            ctx.graph
                .push_instr(block, MirOp::StoreUpvalue { idx: idx.0, val }, pc);
            true
        }
        Instruction::LoadThis { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::LoadThis, pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::CloseUpvalue { local_idx } => {
            ctx.graph
                .push_instr(block, MirOp::CloseUpvalue(local_idx.0), pc);
            true
        }
        Instruction::GetGlobal {
            dst,
            name,
            ic_index,
        } => {
            let v = ctx.graph.push_instr(
                block,
                MirOp::GetGlobal {
                    name_idx: name.0,
                    ic_index: *ic_index,
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::SetGlobal {
            name,
            src,
            ic_index,
            is_declaration: _,
        } => {
            let val = ctx.get_scratch(block, src.0, pc);
            ctx.graph.push_instr(
                block,
                MirOp::SetGlobal {
                    name_idx: name.0,
                    val,
                    ic_index: *ic_index,
                },
                pc,
            );
            true
        }
        Instruction::Add {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => {
            lower_binary_arith(
                ctx,
                block,
                pc,
                BinaryArithRegs {
                    dst: dst.0,
                    lhs: lhs.0,
                    rhs: rhs.0,
                },
                *feedback_index,
                BinaryArithOp::Add,
            );
            true
        }
        Instruction::Sub {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => {
            lower_binary_arith(
                ctx,
                block,
                pc,
                BinaryArithRegs {
                    dst: dst.0,
                    lhs: lhs.0,
                    rhs: rhs.0,
                },
                *feedback_index,
                BinaryArithOp::Sub,
            );
            true
        }
        Instruction::Mul {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => {
            lower_binary_arith(
                ctx,
                block,
                pc,
                BinaryArithRegs {
                    dst: dst.0,
                    lhs: lhs.0,
                    rhs: rhs.0,
                },
                *feedback_index,
                BinaryArithOp::Mul,
            );
            true
        }
        Instruction::Div {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => {
            lower_binary_arith(
                ctx,
                block,
                pc,
                BinaryArithRegs {
                    dst: dst.0,
                    lhs: lhs.0,
                    rhs: rhs.0,
                },
                *feedback_index,
                BinaryArithOp::Div,
            );
            true
        }
        Instruction::AddInt32 {
            dst,
            lhs,
            rhs,
            feedback_index: _,
        } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let result = ctx.graph.push_instr(
                block,
                MirOp::AddI32 {
                    lhs: gl,
                    rhs: gr,
                    deopt,
                },
                pc,
            );
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(result), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::SubInt32 {
            dst,
            lhs,
            rhs,
            feedback_index: _,
        } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let result = ctx.graph.push_instr(
                block,
                MirOp::SubI32 {
                    lhs: gl,
                    rhs: gr,
                    deopt,
                },
                pc,
            );
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(result), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::MulInt32 { dst, lhs, rhs, .. } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let result = ctx.graph.push_instr(
                block,
                MirOp::MulI32 {
                    lhs: gl,
                    rhs: gr,
                    deopt,
                },
                pc,
            );
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(result), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::DivInt32 { dst, lhs, rhs, .. } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let result = ctx.graph.push_instr(
                block,
                MirOp::DivI32 {
                    lhs: gl,
                    rhs: gr,
                    deopt,
                },
                pc,
            );
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(result), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::AddNumber { dst, lhs, rhs, .. }
        | Instruction::SubNumber { dst, lhs, rhs, .. } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx
                .graph
                .push_instr(block, MirOp::GuardFloat64 { val: l, deopt }, pc);
            let gr = ctx
                .graph
                .push_instr(block, MirOp::GuardFloat64 { val: r, deopt }, pc);
            let raw = match inst {
                Instruction::AddNumber { .. } => {
                    ctx.graph
                        .push_instr(block, MirOp::AddF64 { lhs: gl, rhs: gr }, pc)
                }
                _ => ctx
                    .graph
                    .push_instr(block, MirOp::SubF64 { lhs: gl, rhs: gr }, pc),
            };
            let boxed = ctx.graph.push_instr(block, MirOp::BoxFloat64(raw), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::Lt { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind: HelperKind::GenericLt,
                    args: vec![l, r],
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::Le { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind: HelperKind::GenericLe,
                    args: vec![l, r],
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::Gt { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind: HelperKind::GenericGt,
                    args: vec![l, r],
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::Ge { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind: HelperKind::GenericGe,
                    args: vec![l, r],
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::StrictEq { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let v = ctx
                .graph
                .push_instr(block, MirOp::CmpStrictEq { lhs: l, rhs: r }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxBool(v), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::StrictNe { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let v = ctx
                .graph
                .push_instr(block, MirOp::CmpStrictNe { lhs: l, rhs: r }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxBool(v), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::Eq { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind: HelperKind::GenericEq,
                    args: vec![l, r],
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::Ne { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind: HelperKind::GenericEq,
                    args: vec![l, r],
                },
                pc,
            );
            let negated = ctx.graph.push_instr(block, MirOp::LogicalNot(v), pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxBool(negated), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::Not { dst, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            let truthy = ctx.graph.push_instr(block, MirOp::IsTruthy(val), pc);
            let notted = ctx.graph.push_instr(block, MirOp::LogicalNot(truthy), pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxBool(notted), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::Neg { dst, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind: HelperKind::GenericNeg,
                    args: vec![val],
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::Inc { dst, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind: HelperKind::GenericInc,
                    args: vec![val],
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::Dec { dst, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind: HelperKind::GenericDec,
                    args: vec![val],
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::TypeOf { dst, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::TypeOf(val), pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::ToNumber { dst, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::ToNumber(val), pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::ToString { dst, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::ToStringOp(val), pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::RequireCoercible { src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            ctx.graph
                .push_instr(block, MirOp::RequireCoercible(val), pc);
            true
        }
        Instruction::BitAnd { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let v = ctx
                .graph
                .push_instr(block, MirOp::BitAnd { lhs: gl, rhs: gr }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(v), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::BitOr { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let v = ctx
                .graph
                .push_instr(block, MirOp::BitOr { lhs: gl, rhs: gr }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(v), pc);
            ctx.set_scratch(block, dst.0, boxed, pc);
            true
        }
        Instruction::Mod { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind: HelperKind::GenericMod,
                    args: vec![l, r],
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::Pow { dst, lhs, rhs } => {
            let l = ctx.get_scratch(block, lhs.0, pc);
            let r = ctx.get_scratch(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall {
                    kind: HelperKind::Pow,
                    args: vec![l, r],
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::Move { dst, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            ctx.set_scratch(block, dst.0, val, pc);
            true
        }
        _ => false,
    }
}
