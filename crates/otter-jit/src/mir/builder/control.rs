use std::collections::HashMap;

use otter_vm_bytecode::instruction::Instruction;

use crate::mir::graph::BlockId;
use crate::mir::nodes::MirOp;

use super::blocks::resolve_target;
use super::context::BuilderContext;

pub(super) fn lower_instruction(
    ctx: &mut BuilderContext<'_>,
    block: BlockId,
    pc: u32,
    inst: &Instruction,
    pc_to_block: &HashMap<u32, BlockId>,
) -> bool {
    match inst {
        Instruction::Jump { offset } => {
            let target = resolve_target(pc, offset.0, pc_to_block);
            ctx.graph.push_instr(block, MirOp::Jump(target), pc);
            true
        }
        Instruction::JumpIfTrue { cond, offset } => {
            let val = ctx.get_scratch(block, cond.0, pc);
            let truthy = ctx.graph.push_instr(block, MirOp::IsTruthy(val), pc);
            let target = resolve_target(pc, offset.0, pc_to_block);
            let fallthrough = resolve_target(pc, 1, pc_to_block);
            ctx.graph.push_instr(
                block,
                MirOp::Branch {
                    cond: truthy,
                    true_block: target,
                    false_block: fallthrough,
                },
                pc,
            );
            true
        }
        Instruction::JumpIfFalse { cond, offset } => {
            let val = ctx.get_scratch(block, cond.0, pc);
            let truthy = ctx.graph.push_instr(block, MirOp::IsTruthy(val), pc);
            let target = resolve_target(pc, offset.0, pc_to_block);
            let fallthrough = resolve_target(pc, 1, pc_to_block);
            ctx.graph.push_instr(
                block,
                MirOp::Branch {
                    cond: truthy,
                    true_block: fallthrough,
                    false_block: target,
                },
                pc,
            );
            true
        }
        Instruction::Return { src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            ctx.graph.push_instr(block, MirOp::Return(val), pc);
            true
        }
        Instruction::ReturnUndefined => {
            ctx.graph.push_instr(block, MirOp::ReturnUndefined, pc);
            true
        }
        Instruction::Throw { src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            ctx.graph.push_instr(block, MirOp::Throw(val), pc);
            true
        }
        Instruction::TryStart { catch_offset } => {
            let catch_block = resolve_target(pc, catch_offset.0, pc_to_block);
            ctx.graph
                .push_instr(block, MirOp::TryStart { catch_block }, pc);
            true
        }
        Instruction::TryEnd => {
            ctx.graph.push_instr(block, MirOp::TryEnd, pc);
            true
        }
        Instruction::Catch { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::Catch, pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::JumpIfNullish { .. } | Instruction::JumpIfNotNullish { .. } => {
            let deopt = ctx.make_deopt(pc);
            ctx.graph.push_instr(block, MirOp::Deopt(deopt), pc);
            true
        }
        Instruction::Nop | Instruction::Debugger | Instruction::Pop => true,
        Instruction::DeclareGlobalVar { .. } => true,
        _ => false,
    }
}
