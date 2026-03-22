//! Bytecode → MIR lowering.
//!
//! Translates bytecode instructions into MIR operations, reading IC state
//! from the FeedbackVector to decide guard specialization.
//!
//! The builder is a single-pass forward translator. It maintains a virtual
//! register file mapping bytecode registers to MIR ValueIds. For each
//! bytecode instruction, it emits the corresponding MIR operations.

use otter_vm_bytecode::function::ArithmeticType;
use otter_vm_bytecode::instruction::Instruction;
use otter_vm_bytecode::Function;

use crate::feedback::{FeedbackSnapshot, IcSnapshot};
use crate::mir::graph::{BlockId, DeoptId, DeoptInfo, MirGraph, ResumeMode, ValueId};
use crate::mir::nodes::{HelperKind, MirOp};
use crate::mir::types::CmpOp;

/// Build a MIR graph from a bytecode function.
pub fn build_mir(function: &Function) -> MirGraph {
    let feedback = FeedbackSnapshot::from_function(function);
    let name = function.name.as_deref().unwrap_or("<anonymous>").to_string();
    let local_count = function.local_count;
    let register_count = function.register_count;
    let param_count = function.param_count;

    let mut ctx = BuilderContext::new(name, local_count, register_count, param_count as u16, &feedback);
    let instructions = function.instructions.read();

    // First pass: identify jump targets to split basic blocks.
    let block_starts = find_block_starts(instructions);

    // Map bytecode PC → BlockId.
    let mut pc_to_block = std::collections::HashMap::new();
    for &pc in &block_starts {
        if pc == 0 {
            pc_to_block.insert(0u32, ctx.graph.entry_block);
        } else {
            let bid = ctx.graph.create_block();
            pc_to_block.insert(pc as u32, bid);
        }
    }

    // Second pass: lower each instruction.
    let mut current_block = ctx.graph.entry_block;
    for (pc, inst) in instructions.iter().enumerate() {
        let pc = pc as u32;

        // Start a new block if this PC is a block boundary.
        if let Some(&bid) = pc_to_block.get(&pc) {
            if bid != current_block {
                // If the previous block doesn't have a terminator, add a fallthrough jump.
                if !ctx.graph.block(current_block).is_terminated() {
                    ctx.graph.push_instr(current_block, MirOp::Jump(bid), pc);
                }
                current_block = bid;
            }
        }

        lower_instruction(&mut ctx, current_block, pc, inst, &pc_to_block);

        // If we just emitted a terminator, the next instruction starts a new block.
        if ctx.graph.block(current_block).is_terminated() {
            if let Some(&next_bid) = pc_to_block.get(&(pc + 1)) {
                current_block = next_bid;
            } else {
                // Create a dead-code block for any remaining instructions.
                current_block = ctx.graph.create_block();
            }
        }
    }

    ctx.graph.recompute_edges();
    ctx.graph
}

/// Builder context holding state during MIR construction.
struct BuilderContext<'a> {
    graph: MirGraph,
    feedback: &'a FeedbackSnapshot,
    /// Maps bytecode register index → current MIR ValueId.
    /// Registers are virtual: local 0..N then scratch 0..K.
    reg_map: Vec<Option<ValueId>>,
}

impl<'a> BuilderContext<'a> {
    fn new(
        name: String,
        local_count: u16,
        register_count: u16,
        param_count: u16,
        feedback: &'a FeedbackSnapshot,
    ) -> Self {
        let total_regs = register_count as usize;
        Self {
            graph: MirGraph::new(name, local_count, register_count, param_count),
            feedback,
            reg_map: vec![None; total_regs],
        }
    }

    /// Get the MIR value for a bytecode register. If not yet loaded, emit a LoadLocal/LoadRegister.
    fn get_reg(&mut self, block: BlockId, reg: u16, pc: u32) -> ValueId {
        if let Some(val) = self.reg_map[reg as usize] {
            return val;
        }
        // First use — emit a load from the register window.
        let local_count = self.graph.local_count;
        let val = if reg < local_count {
            self.graph.push_instr(block, MirOp::LoadLocal(reg), pc)
        } else {
            self.graph
                .push_instr(block, MirOp::LoadRegister(reg - local_count), pc)
        };
        self.reg_map[reg as usize] = Some(val);
        val
    }

    /// Set the MIR value for a bytecode register.
    fn set_reg(&mut self, block: BlockId, reg: u16, val: ValueId, pc: u32) {
        self.reg_map[reg as usize] = Some(val);
        let local_count = self.graph.local_count;
        if reg < local_count {
            self.graph
                .push_instr(block, MirOp::StoreLocal { idx: reg, val }, pc);
        } else {
            self.graph.push_instr(
                block,
                MirOp::StoreRegister {
                    idx: reg - local_count,
                    val,
                },
                pc,
            );
        }
    }

    /// Create a deopt point that resumes at the given bytecode PC.
    fn make_deopt(&mut self, pc: u32) -> DeoptId {
        self.graph.create_deopt(DeoptInfo {
            bytecode_pc: pc,
            live_state: Vec::new(),
            resume_mode: ResumeMode::ResumeAtPc,
        })
    }
}

/// Find bytecode PCs that start basic blocks (jump targets, after jumps, after conditionals).
fn find_block_starts(instructions: &[Instruction]) -> Vec<usize> {
    let mut starts = std::collections::BTreeSet::new();
    starts.insert(0); // Entry block.

    for (pc, inst) in instructions.iter().enumerate() {
        match inst {
            Instruction::Jump { offset } => {
                let target = (pc as i64 + offset.0 as i64) as usize;
                starts.insert(target);
                starts.insert(pc + 1); // Fallthrough (likely dead but needed for structure).
            }
            Instruction::JumpIfTrue { offset, .. }
            | Instruction::JumpIfFalse { offset, .. }
            | Instruction::JumpIfNullish { offset, .. }
            | Instruction::JumpIfNotNullish { offset, .. } => {
                let target = (pc as i64 + offset.0 as i64) as usize;
                starts.insert(target);
                starts.insert(pc + 1);
            }
            Instruction::TryStart { catch_offset } => {
                let target = (pc as i64 + catch_offset.0 as i64) as usize;
                starts.insert(target);
                starts.insert(pc + 1);
            }
            Instruction::Return { .. } | Instruction::ReturnUndefined | Instruction::Throw { .. } => {
                starts.insert(pc + 1);
            }
            _ => {}
        }
    }

    starts.into_iter().collect()
}

/// Resolve a jump target PC to a BlockId.
fn resolve_target(
    pc: u32,
    offset: i32,
    pc_to_block: &std::collections::HashMap<u32, BlockId>,
) -> BlockId {
    let target = (pc as i64 + offset as i64) as u32;
    *pc_to_block
        .get(&target)
        .unwrap_or_else(|| panic!("jump target pc={} not mapped to a block", target))
}

/// Lower a single bytecode instruction to MIR.
fn lower_instruction(
    ctx: &mut BuilderContext,
    block: BlockId,
    pc: u32,
    inst: &Instruction,
    pc_to_block: &std::collections::HashMap<u32, BlockId>,
) {
    match inst {
        // ---- Constants ----
        Instruction::LoadUndefined { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::Undefined, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::LoadNull { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::Null, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::LoadTrue { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::True, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::LoadFalse { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::False, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::LoadInt8 { dst, value } => {
            let v = ctx
                .graph
                .push_instr(block, MirOp::ConstInt32(*value as i32), pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(v), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }
        Instruction::LoadInt32 { dst, value } => {
            let v = ctx.graph.push_instr(block, MirOp::ConstInt32(*value), pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(v), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }
        Instruction::LoadConst { dst, idx } => {
            let v = ctx
                .graph
                .push_instr(block, MirOp::LoadConstPool(idx.0), pc);
            ctx.set_reg(block, dst.0, v, pc);
        }

        // ---- Variables ----
        Instruction::GetLocal { dst, idx } => {
            let v = ctx
                .graph
                .push_instr(block, MirOp::LoadLocal(idx.0), pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::SetLocal { idx, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            ctx.graph
                .push_instr(block, MirOp::StoreLocal { idx: idx.0, val }, pc);
        }
        Instruction::GetUpvalue { dst, idx } => {
            let v = ctx
                .graph
                .push_instr(block, MirOp::LoadUpvalue(idx.0), pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::SetUpvalue { idx, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            ctx.graph.push_instr(
                block,
                MirOp::StoreUpvalue {
                    idx: idx.0,
                    val,
                },
                pc,
            );
        }
        Instruction::LoadThis { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::LoadThis, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::CloseUpvalue { local_idx } => {
            ctx.graph
                .push_instr(block, MirOp::CloseUpvalue(local_idx.0), pc);
        }

        // ---- Globals ----
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
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::SetGlobal {
            name,
            src,
            ic_index,
            is_declaration: _,
        } => {
            let val = ctx.get_reg(block, src.0, pc);
            ctx.graph.push_instr(
                block,
                MirOp::SetGlobal {
                    name_idx: name.0,
                    val,
                    ic_index: *ic_index,
                },
                pc,
            );
        }

        // ---- Arithmetic (with IC-guided specialization) ----
        Instruction::Add {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => {
            lower_binary_arith(ctx, block, pc, dst.0, lhs.0, rhs.0, *feedback_index, BinaryArithOp::Add);
        }
        Instruction::Sub {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => {
            lower_binary_arith(ctx, block, pc, dst.0, lhs.0, rhs.0, *feedback_index, BinaryArithOp::Sub);
        }
        Instruction::Mul {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => {
            lower_binary_arith(ctx, block, pc, dst.0, lhs.0, rhs.0, *feedback_index, BinaryArithOp::Mul);
        }
        Instruction::Div {
            dst,
            lhs,
            rhs,
            feedback_index,
        } => {
            lower_binary_arith(ctx, block, pc, dst.0, lhs.0, rhs.0, *feedback_index, BinaryArithOp::Div);
        }

        // Quickened int32 arithmetic (already specialized by interpreter).
        Instruction::AddInt32 {
            dst,
            lhs,
            rhs,
            feedback_index: _,
        } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let result = ctx.graph.push_instr(block, MirOp::AddI32 { lhs: gl, rhs: gr, deopt }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(result), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }
        Instruction::SubInt32 {
            dst,
            lhs,
            rhs,
            feedback_index: _,
        } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let result = ctx.graph.push_instr(block, MirOp::SubI32 { lhs: gl, rhs: gr, deopt }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(result), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }
        Instruction::MulInt32 { dst, lhs, rhs, .. } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let result = ctx.graph.push_instr(block, MirOp::MulI32 { lhs: gl, rhs: gr, deopt }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(result), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }

        // ---- Comparisons ----
        Instruction::Lt { dst, lhs, rhs } | Instruction::Le { dst, lhs, rhs }
        | Instruction::Gt { dst, lhs, rhs } | Instruction::Ge { dst, lhs, rhs } => {
            let cmp_op = match inst {
                Instruction::Lt { .. } => CmpOp::Lt,
                Instruction::Le { .. } => CmpOp::Le,
                Instruction::Gt { .. } => CmpOp::Gt,
                Instruction::Ge { .. } => CmpOp::Ge,
                _ => unreachable!(),
            };
            let helper = match cmp_op {
                CmpOp::Lt => HelperKind::GenericLt,
                CmpOp::Le => HelperKind::GenericLe,
                CmpOp::Gt => HelperKind::GenericGt,
                CmpOp::Ge => HelperKind::GenericGe,
                _ => unreachable!(),
            };
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::HelperCall { kind: helper, args: vec![l, r] }, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::StrictEq { dst, lhs, rhs } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::CmpStrictEq { lhs: l, rhs: r }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxBool(v), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }
        Instruction::StrictNe { dst, lhs, rhs } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::CmpStrictNe { lhs: l, rhs: r }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxBool(v), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }

        // ---- Unary ----
        Instruction::Not { dst, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            let truthy = ctx.graph.push_instr(block, MirOp::IsTruthy(val), pc);
            let notted = ctx.graph.push_instr(block, MirOp::LogicalNot(truthy), pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxBool(notted), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }
        Instruction::Neg { dst, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::HelperCall { kind: HelperKind::GenericNeg, args: vec![val] }, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::Inc { dst, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::HelperCall { kind: HelperKind::GenericInc, args: vec![val] }, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::Dec { dst, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::HelperCall { kind: HelperKind::GenericDec, args: vec![val] }, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::TypeOf { dst, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::TypeOf(val), pc);
            ctx.set_reg(block, dst.0, v, pc);
        }

        // ---- Property Access ----
        Instruction::GetPropConst {
            dst,
            obj,
            name,
            ic_index,
        } => {
            lower_get_prop_const(ctx, block, pc, dst.0, obj.0, name.0, *ic_index);
        }
        Instruction::SetPropConst {
            obj,
            name,
            val,
            ic_index,
        } => {
            lower_set_prop_const(ctx, block, pc, obj.0, name.0, val.0, *ic_index);
        }
        Instruction::GetProp {
            dst,
            obj,
            key,
            ic_index,
        } => {
            let o = ctx.get_reg(block, obj.0, pc);
            let k = ctx.get_reg(block, key.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::GetPropGeneric { obj: o, key: k, ic_index: *ic_index },
                pc,
            );
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::SetProp { obj, key, val, ic_index } => {
            let o = ctx.get_reg(block, obj.0, pc);
            let k = ctx.get_reg(block, key.0, pc);
            let v = ctx.get_reg(block, val.0, pc);
            ctx.graph.push_instr(
                block,
                MirOp::SetPropGeneric { obj: o, key: k, val: v, ic_index: *ic_index },
                pc,
            );
        }

        // ---- Array / Element Access ----
        Instruction::NewArray { dst, len, .. } => {
            let v = ctx
                .graph
                .push_instr(block, MirOp::NewArray { len: *len }, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::GetElem { dst, arr, idx, ic_index } => {
            let o = ctx.get_reg(block, arr.0, pc);
            let k = ctx.get_reg(block, idx.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::GetElemGeneric { obj: o, key: k, ic_index: *ic_index },
                pc,
            );
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::SetElem { arr, idx, val, ic_index } => {
            let o = ctx.get_reg(block, arr.0, pc);
            let k = ctx.get_reg(block, idx.0, pc);
            let v = ctx.get_reg(block, val.0, pc);
            ctx.graph.push_instr(
                block,
                MirOp::SetElemGeneric { obj: o, key: k, val: v, ic_index: *ic_index },
                pc,
            );
        }

        // ---- Calls ----
        Instruction::Call {
            dst,
            func,
            argc,
            ic_index,
        } => {
            let callee_val = ctx.get_reg(block, func.0, pc);
            // Collect arguments from registers following callee.
            let args: Vec<ValueId> = (0..*argc)
                .map(|i| ctx.get_reg(block, func.0 + 1 + i as u16, pc))
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
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::CallMethod {
            dst,
            obj,
            method,
            argc,
            ic_index,
        } => {
            let obj_val = ctx.get_reg(block, obj.0, pc);
            let args: Vec<ValueId> = (0..*argc)
                .map(|i| ctx.get_reg(block, obj.0 + 1 + i as u16, pc))
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
            ctx.set_reg(block, dst.0, v, pc);
        }

        // ---- Object Creation ----
        Instruction::NewObject { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::NewObject, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::Closure { dst, func } => {
            let v = ctx
                .graph
                .push_instr(block, MirOp::CreateClosure { func_idx: func.0 }, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::DefineProperty { obj, key, val } => {
            let o = ctx.get_reg(block, obj.0, pc);
            let k = ctx.get_reg(block, key.0, pc);
            let v = ctx.get_reg(block, val.0, pc);
            ctx.graph.push_instr(block, MirOp::DefineProperty { obj: o, key: k, val: v }, pc);
        }

        // ---- Control Flow ----
        Instruction::Jump { offset } => {
            let target = resolve_target(pc, offset.0, pc_to_block);
            ctx.graph.push_instr(block, MirOp::Jump(target), pc);
        }
        Instruction::JumpIfTrue { cond, offset } => {
            let val = ctx.get_reg(block, cond.0, pc);
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
        }
        Instruction::JumpIfFalse { cond, offset } => {
            let val = ctx.get_reg(block, cond.0, pc);
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
        }
        Instruction::Return { src } => {
            let val = ctx.get_reg(block, src.0, pc);
            ctx.graph.push_instr(block, MirOp::Return(val), pc);
        }
        Instruction::ReturnUndefined => {
            ctx.graph.push_instr(block, MirOp::ReturnUndefined, pc);
        }
        Instruction::Throw { src } => {
            let val = ctx.get_reg(block, src.0, pc);
            ctx.graph.push_instr(block, MirOp::Throw(val), pc);
        }

        // ---- Move ----
        Instruction::Move { dst, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            ctx.set_reg(block, dst.0, val, pc);
        }

        // ---- Exception Handling ----
        Instruction::TryStart { catch_offset } => {
            let catch_block = resolve_target(pc, catch_offset.0, pc_to_block);
            ctx.graph.push_instr(block, MirOp::TryStart { catch_block }, pc);
        }
        Instruction::TryEnd => {
            ctx.graph.push_instr(block, MirOp::TryEnd, pc);
        }
        Instruction::Catch { dst } => {
            let v = ctx.graph.push_instr(block, MirOp::Catch, pc);
            ctx.set_reg(block, dst.0, v, pc);
        }

        // ---- Iteration ----
        Instruction::GetIterator { dst, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::GetIterator(val), pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::IteratorNext { dst, done: _, iter } => {
            let it = ctx.get_reg(block, iter.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::IteratorNext(it), pc);
            ctx.set_reg(block, dst.0, v, pc);
            // Done flag is handled via secondary_result in the helper.
            // For now, emit a placeholder. The codegen will read secondary_result.
        }
        Instruction::IteratorClose { iter } => {
            let it = ctx.get_reg(block, iter.0, pc);
            ctx.graph.push_instr(block, MirOp::IteratorClose(it), pc);
        }

        // ---- Type Operations ----
        Instruction::ToNumber { dst, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::ToNumber(val), pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::ToString { dst, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::ToStringOp(val), pc);
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::RequireCoercible { src } => {
            let val = ctx.get_reg(block, src.0, pc);
            ctx.graph.push_instr(block, MirOp::RequireCoercible(val), pc);
        }

        // ---- Nop / Debugger / DeclareGlobalVar ----
        Instruction::Nop | Instruction::Debugger | Instruction::Pop => {}
        Instruction::DeclareGlobalVar { .. } => {
            // Global var declarations are handled at module init time.
            // In JIT code this is a no-op.
        }

        // ---- Quickened instructions we don't handle yet ----
        Instruction::AddNumber { dst, lhs, rhs, .. }
        | Instruction::SubNumber { dst, lhs, rhs, .. } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
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
                _ => {
                    ctx.graph
                        .push_instr(block, MirOp::SubF64 { lhs: gl, rhs: gr }, pc)
                }
            };
            let boxed = ctx.graph.push_instr(block, MirOp::BoxFloat64(raw), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }
        Instruction::DivInt32 { dst, lhs, rhs, .. } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx
                .graph
                .push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let result = ctx
                .graph
                .push_instr(block, MirOp::DivI32 { lhs: gl, rhs: gr, deopt }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(result), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }

        // ---- Eq / Ne (generic) ----
        Instruction::Eq { dst, lhs, rhs } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall { kind: HelperKind::GenericEq, args: vec![l, r] },
                pc,
            );
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::Ne { dst, lhs, rhs } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall { kind: HelperKind::GenericEq, args: vec![l, r] },
                pc,
            );
            // Negate the result
            let negated = ctx.graph.push_instr(block, MirOp::LogicalNot(v), pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxBool(negated), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }

        // ---- Bitwise ----
        Instruction::BitAnd { dst, lhs, rhs } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let v = ctx.graph.push_instr(block, MirOp::BitAnd { lhs: gl, rhs: gr }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(v), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }
        Instruction::BitOr { dst, lhs, rhs } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let deopt = ctx.make_deopt(pc);
            let gl = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let v = ctx.graph.push_instr(block, MirOp::BitOr { lhs: gl, rhs: gr }, pc);
            let boxed = ctx.graph.push_instr(block, MirOp::BoxInt32(v), pc);
            ctx.set_reg(block, dst.0, boxed, pc);
        }

        // ---- Mod / Pow (generic) ----
        Instruction::Mod { dst, lhs, rhs } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall { kind: HelperKind::GenericMod, args: vec![l, r] },
                pc,
            );
            ctx.set_reg(block, dst.0, v, pc);
        }
        Instruction::Pow { dst, lhs, rhs } => {
            let l = ctx.get_reg(block, lhs.0, pc);
            let r = ctx.get_reg(block, rhs.0, pc);
            let v = ctx.graph.push_instr(
                block,
                MirOp::HelperCall { kind: HelperKind::Pow, args: vec![l, r] },
                pc,
            );
            ctx.set_reg(block, dst.0, v, pc);
        }

        // ---- Spread ----
        Instruction::Spread { dst, src } => {
            let val = ctx.get_reg(block, src.0, pc);
            let v = ctx.graph.push_instr(block, MirOp::Spread(val), pc);
            ctx.set_reg(block, dst.0, v, pc);
        }

        // ---- SetPrototype ----
        Instruction::SetPrototype { obj, proto } => {
            let o = ctx.get_reg(block, obj.0, pc);
            let p = ctx.get_reg(block, proto.0, pc);
            ctx.graph.push_instr(block, MirOp::SetPrototype { obj: o, proto: p }, pc);
        }

        // ---- JumpIfNullish / JumpIfNotNullish ----
        Instruction::JumpIfNullish { src: _, offset: _ } => {
            // Deopt for now — this is uncommon in hot code.
            let deopt = ctx.make_deopt(pc);
            ctx.graph.push_instr(block, MirOp::Deopt(deopt), pc);
        }
        Instruction::JumpIfNotNullish { src: _, offset: _ } => {
            let deopt = ctx.make_deopt(pc);
            ctx.graph.push_instr(block, MirOp::Deopt(deopt), pc);
        }

        // ---- Everything else: deopt to interpreter ----
        _ => {
            let deopt = ctx.make_deopt(pc);
            ctx.graph.push_instr(block, MirOp::Deopt(deopt), pc);
        }
    }
}

// ---- Helpers for IC-guided specialization ----

#[derive(Clone, Copy)]
enum BinaryArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

fn lower_binary_arith(
    ctx: &mut BuilderContext,
    block: BlockId,
    pc: u32,
    dst: u16,
    lhs: u16,
    rhs: u16,
    feedback_index: u16,
    op: BinaryArithOp,
) {
    let l = ctx.get_reg(block, lhs, pc);
    let r = ctx.get_reg(block, rhs, pc);

    // Check IC state for specialization.
    let ic = ctx.feedback.ic(feedback_index);

    let result = match ic {
        IcSnapshot::Arithmetic(ArithmeticType::Int32) => {
            let deopt = ctx.make_deopt(pc);
            let gl = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: l, deopt }, pc);
            let gr = ctx.graph.push_instr(block, MirOp::GuardInt32 { val: r, deopt }, pc);
            let raw = match op {
                BinaryArithOp::Add => ctx.graph.push_instr(block, MirOp::AddI32 { lhs: gl, rhs: gr, deopt }, pc),
                BinaryArithOp::Sub => ctx.graph.push_instr(block, MirOp::SubI32 { lhs: gl, rhs: gr, deopt }, pc),
                BinaryArithOp::Mul => ctx.graph.push_instr(block, MirOp::MulI32 { lhs: gl, rhs: gr, deopt }, pc),
                BinaryArithOp::Div => ctx.graph.push_instr(block, MirOp::DivI32 { lhs: gl, rhs: gr, deopt }, pc),
            };
            ctx.graph.push_instr(block, MirOp::BoxInt32(raw), pc)
        }
        IcSnapshot::Arithmetic(ArithmeticType::Number) => {
            let deopt = ctx.make_deopt(pc);
            let gl = ctx.graph.push_instr(block, MirOp::GuardFloat64 { val: l, deopt }, pc);
            let gr = ctx.graph.push_instr(block, MirOp::GuardFloat64 { val: r, deopt }, pc);
            let raw = match op {
                BinaryArithOp::Add => ctx.graph.push_instr(block, MirOp::AddF64 { lhs: gl, rhs: gr }, pc),
                BinaryArithOp::Sub => ctx.graph.push_instr(block, MirOp::SubF64 { lhs: gl, rhs: gr }, pc),
                BinaryArithOp::Mul => ctx.graph.push_instr(block, MirOp::MulF64 { lhs: gl, rhs: gr }, pc),
                BinaryArithOp::Div => ctx.graph.push_instr(block, MirOp::DivF64 { lhs: gl, rhs: gr }, pc),
            };
            ctx.graph.push_instr(block, MirOp::BoxFloat64(raw), pc)
        }
        _ => {
            // Generic: use helper call.
            let kind = match op {
                BinaryArithOp::Add => HelperKind::GenericAdd,
                BinaryArithOp::Sub => HelperKind::GenericSub,
                BinaryArithOp::Mul => HelperKind::GenericMul,
                BinaryArithOp::Div => HelperKind::GenericDiv,
            };
            ctx.graph.push_instr(block, MirOp::HelperCall { kind, args: vec![l, r] }, pc)
        }
    };

    ctx.set_reg(block, dst, result, pc);
}

fn lower_get_prop_const(
    ctx: &mut BuilderContext,
    block: BlockId,
    pc: u32,
    dst: u16,
    obj: u16,
    name_idx: u32,
    ic_index: u16,
) {
    let obj_val = ctx.get_reg(block, obj, pc);
    let ic = ctx.feedback.ic(ic_index);

    let result = match ic {
        IcSnapshot::MonoProp {
            shape_id,
            offset,
            depth: 0,
            ..
        } => {
            // Monomorphic own-property: guard shape, then inline load.
            let deopt = ctx.make_deopt(pc);
            let obj_ref = ctx.graph.push_instr(block, MirOp::GuardObject { val: obj_val, deopt }, pc);
            ctx.graph.push_instr(block, MirOp::GuardShape { obj: obj_ref, shape_id: *shape_id, deopt }, pc);
            let inline = *offset < 8;
            ctx.graph.push_instr(block, MirOp::GetPropShaped { obj: obj_ref, offset: *offset, inline }, pc)
        }
        _ => {
            // Generic fallback.
            ctx.graph.push_instr(
                block,
                MirOp::GetPropConstGeneric {
                    obj: obj_val,
                    name_idx,
                    ic_index,
                },
                pc,
            )
        }
    };

    ctx.set_reg(block, dst, result, pc);
}

fn lower_set_prop_const(
    ctx: &mut BuilderContext,
    block: BlockId,
    pc: u32,
    obj: u16,
    name_idx: u32,
    val: u16,
    ic_index: u16,
) {
    let obj_val = ctx.get_reg(block, obj, pc);
    let set_val = ctx.get_reg(block, val, pc);
    let ic = ctx.feedback.ic(ic_index);

    match ic {
        IcSnapshot::MonoProp {
            shape_id,
            offset,
            depth: 0,
            ..
        } => {
            let deopt = ctx.make_deopt(pc);
            let obj_ref = ctx.graph.push_instr(block, MirOp::GuardObject { val: obj_val, deopt }, pc);
            ctx.graph.push_instr(block, MirOp::GuardShape { obj: obj_ref, shape_id: *shape_id, deopt }, pc);
            let inline = *offset < 8;
            ctx.graph.push_instr(block, MirOp::SetPropShaped { obj: obj_ref, offset: *offset, val: set_val, inline }, pc);
            // Write barrier for heap stores.
            ctx.graph.push_instr(block, MirOp::WriteBarrier(set_val), pc);
        }
        _ => {
            ctx.graph.push_instr(
                block,
                MirOp::SetPropConstGeneric {
                    obj: obj_val,
                    name_idx,
                    val: set_val,
                    ic_index,
                },
                pc,
            );
        }
    }
}
