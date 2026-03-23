use std::collections::HashMap;

use otter_vm_bytecode::instruction::Instruction;

use crate::feedback::IcSnapshot;
use crate::mir::graph::BlockId;
use crate::mir::nodes::MirOp;

use super::context::BuilderContext;

pub(super) fn lower_instruction(
    ctx: &mut BuilderContext<'_>,
    block: BlockId,
    pc: u32,
    inst: &Instruction,
    _pc_to_block: &HashMap<u32, BlockId>,
) -> bool {
    match inst {
        Instruction::GetPropConst {
            dst,
            obj,
            name,
            ic_index,
        } => {
            let obj_val = ctx.get_scratch(block, obj.0, pc);
            let ic = ctx.feedback.ic(*ic_index);
            let result = match ic {
                IcSnapshot::MonoProp {
                    shape_id,
                    offset,
                    depth: 0,
                    ..
                } => {
                    let deopt = ctx.make_deopt(pc);
                    let obj_ref = ctx.graph.push_instr(
                        block,
                        MirOp::GuardObject {
                            val: obj_val,
                            deopt,
                        },
                        pc,
                    );
                    ctx.graph.push_instr(
                        block,
                        MirOp::GuardShape {
                            obj: obj_ref,
                            shape_id: *shape_id,
                            deopt,
                        },
                        pc,
                    );
                    let inline = *offset < 8;
                    ctx.graph.push_instr(
                        block,
                        MirOp::GetPropShaped {
                            obj: obj_ref,
                            shape_id: *shape_id,
                            offset: *offset,
                            inline,
                        },
                        pc,
                    )
                }
                _ => ctx.graph.push_instr(
                    block,
                    MirOp::GetPropConstGeneric {
                        obj: obj_val,
                        name_idx: name.0,
                        ic_index: *ic_index,
                    },
                    pc,
                ),
            };
            ctx.set_scratch(block, dst.0, result, pc);
            true
        }
        Instruction::SetPropConst {
            obj,
            name,
            val,
            ic_index,
        } => {
            let obj_val = ctx.get_scratch(block, obj.0, pc);
            let set_val = ctx.get_scratch(block, val.0, pc);
            let ic = ctx.feedback.ic(*ic_index);
            match ic {
                IcSnapshot::MonoProp {
                    shape_id,
                    offset,
                    depth: 0,
                    ..
                } => {
                    let deopt = ctx.make_deopt(pc);
                    let obj_ref = ctx.graph.push_instr(
                        block,
                        MirOp::GuardObject {
                            val: obj_val,
                            deopt,
                        },
                        pc,
                    );
                    ctx.graph.push_instr(
                        block,
                        MirOp::GuardShape {
                            obj: obj_ref,
                            shape_id: *shape_id,
                            deopt,
                        },
                        pc,
                    );
                    let inline = *offset < 8;
                    ctx.graph.push_instr(
                        block,
                        MirOp::SetPropShaped {
                            obj: obj_ref,
                            shape_id: *shape_id,
                            offset: *offset,
                            val: set_val,
                            inline,
                        },
                        pc,
                    );
                    ctx.graph
                        .push_instr(block, MirOp::WriteBarrier(set_val), pc);
                }
                _ => {
                    ctx.graph.push_instr(
                        block,
                        MirOp::SetPropConstGeneric {
                            obj: obj_val,
                            name_idx: name.0,
                            val: set_val,
                            ic_index: *ic_index,
                        },
                        pc,
                    );
                }
            }
            true
        }
        Instruction::GetProp {
            dst,
            obj,
            key,
            ic_index,
        } => {
            let o = ctx.get_scratch(block, obj.0, pc);
            let k = ctx.get_scratch(block, key.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::GetPropGeneric {
                    obj: o,
                    key: k,
                    ic_index: *ic_index,
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::SetProp {
            obj,
            key,
            val,
            ic_index,
        } => {
            let o = ctx.get_scratch(block, obj.0, pc);
            let k = ctx.get_scratch(block, key.0, pc);
            let v = ctx.get_scratch(block, val.0, pc);
            ctx.graph.push_instr(
                block,
                MirOp::SetPropGeneric {
                    obj: o,
                    key: k,
                    val: v,
                    ic_index: *ic_index,
                },
                pc,
            );
            true
        }
        Instruction::NewArray { dst, len, .. } => {
            let v = ctx
                .graph
                .push_instr(block, MirOp::NewArray { len: *len }, pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::GetElem {
            dst,
            arr,
            idx,
            ic_index,
        } => {
            let o = ctx.get_scratch(block, arr.0, pc);
            let k = ctx.get_scratch(block, idx.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::GetElemGeneric {
                    obj: o,
                    key: k,
                    ic_index: *ic_index,
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::SetElem {
            arr,
            idx,
            val,
            ic_index,
        } => {
            let o = ctx.get_scratch(block, arr.0, pc);
            let k = ctx.get_scratch(block, idx.0, pc);
            let v = ctx.get_scratch(block, val.0, pc);
            ctx.graph.push_instr(
                block,
                MirOp::SetElemGeneric {
                    obj: o,
                    key: k,
                    val: v,
                    ic_index: *ic_index,
                },
                pc,
            );
            true
        }
        Instruction::Call {
            dst,
            func,
            argc,
            ic_index,
        } => {
            let callee_val = ctx.get_scratch(block, func.0, pc);
            let args = (0..*argc)
                .map(|i| ctx.get_scratch(block, func.0 + 1 + i as u16, pc))
                .collect();
            let v = ctx.graph.push_instr(
                block,
                MirOp::CallGeneric {
                    callee: callee_val,
                    args,
                    ic_index: *ic_index,
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::CallMethod {
            dst,
            obj,
            method,
            argc,
            ic_index,
        } => {
            let obj_val = ctx.get_scratch(block, obj.0, pc);
            let args = (0..*argc)
                .map(|i| ctx.get_scratch(block, obj.0 + 1 + i as u16, pc))
                .collect();
            let v = ctx.graph.push_instr(
                block,
                MirOp::CallMethodGeneric {
                    obj: obj_val,
                    name_idx: method.0,
                    args,
                    ic_index: *ic_index,
                },
                pc,
            );
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::NewObject { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::NewObject, pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::Closure { dst, func } => {
            let v = ctx
                .graph
                .push_instr(block, MirOp::CreateClosure { func_idx: func.0 }, pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::DefineProperty { obj, key, val } => {
            let o = ctx.get_scratch(block, obj.0, pc);
            let k = ctx.get_scratch(block, key.0, pc);
            let v = ctx.get_scratch(block, val.0, pc);
            ctx.graph.push_instr(
                block,
                MirOp::DefineProperty {
                    obj: o,
                    key: k,
                    val: v,
                },
                pc,
            );
            true
        }
        Instruction::GetIterator { dst, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::GetIterator(val), pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::IteratorNext { dst, done: _, iter } => {
            let it = ctx.get_scratch(block, iter.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::IteratorNext(it), pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::IteratorClose { iter } => {
            let it = ctx.get_scratch(block, iter.0, pc);
            ctx.graph.push_instr(block, MirOp::IteratorClose(it), pc);
            true
        }
        Instruction::Spread { dst, src } => {
            let val = ctx.get_scratch(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::Spread(val), pc);
            ctx.set_scratch(block, dst.0, v, pc);
            true
        }
        Instruction::SetPrototype { obj, proto } => {
            let o = ctx.get_scratch(block, obj.0, pc);
            let p = ctx.get_scratch(block, proto.0, pc);
            ctx.graph
                .push_instr(block, MirOp::SetPrototype { obj: o, proto: p }, pc);
            true
        }
        _ => false,
    }
}
