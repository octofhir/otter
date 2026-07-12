//! AArch64 call, inlining, and collection-method emitters.
//!
//! # Contents
//! - Plain, closure, method, and polymorphic inline eligibility/emission.
//! - Self-recursive and direct-call frame/window transitions.
//! - Collection leaf/allocating guards and live method IC dispatch.
//! - Array method fast paths reached through method calls.
//!
//! # Invariants
//! - Inlined bodies remain non-observable before any possible bailout.
//! - Direct calls publish exact frame windows and restore caller state.
//! - Allocating collection paths use planned safepoints and barriers.
//! - Guard misses preserve the original interpreter fallback semantics.

use super::*;

/// Largest callee register window the inliner accepts. Bounds the per-site
/// scratch reservation and keeps a spliced body "tiny".
const INLINE_MAX_REGS: u16 = 24;
/// Largest callee instruction count the inliner accepts.
const INLINE_MAX_INSTRS: usize = 48;
/// Largest argument count an inlined call accepts.
const INLINE_MAX_ARGS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InlineCallKind {
    Plain,
    ClosureUpvalues,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum InlineKnown {
    Unknown,
    Number,
    Bool,
}

/// Whether an op may appear in an inlined leaf callee: a pure, non-allocating
/// operation with no `this`/upvalue/global/heap access and no further call,
/// so the spliced body has no GC point and commits nothing observable before
/// it can bail. Any op outside this set aborts the inline attempt.
pub(super) fn is_inline_pure_op(op: Op) -> bool {
    matches!(
        op,
        Op::LoadInt32
            | Op::LoadNumber
            | Op::LoadLocal
            | Op::LoadUndefined
            | Op::LoadNull
            | Op::LoadHole
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::StoreLocal
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::BitwiseOr
            | Op::BitwiseAnd
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
            | Op::Ushr
            | Op::LessThan
            | Op::LessEq
            | Op::GreaterThan
            | Op::GreaterEq
            | Op::Equal
            | Op::NotEqual
            | Op::ToPrimitive
            | Op::ToNumeric
            | Op::Jump
            | Op::JumpIfFalse
            | Op::JumpIfTrue
            | Op::Return
            | Op::ReturnValue
            | Op::ReturnUndefined
    )
}

pub(super) fn inline_plain_op_allowed(
    code_block: &otter_vm::CodeBlock,
    instr: &otter_vm::JitInstructionMetadata,
) -> bool {
    is_inline_pure_op(instr.op(code_block))
        || (matches!(instr.op(code_block), Op::MakeFunction | Op::MakeClosure) && instr.make_self)
}

pub(super) fn self_bindings_are_dead(callee: &JitInlineCallee) -> bool {
    let mut pending = Vec::<u16>::new();
    let code_block = callee.code_block.as_ref();

    for instr in &callee.instructions {
        let operands = InstructionOperandView {
            code_block,
            instruction: instr,
        };
        let mut ok = true;
        match instr.op(code_block) {
            Op::LoadLocal | Op::StoreLocal => {}
            Op::ToPrimitive | Op::ToNumeric => {
                ok &= reg(operands, 1)
                    .ok()
                    .is_some_and(|regn| !pending.contains(&regn));
            }
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::BitwiseOr
            | Op::BitwiseAnd
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
            | Op::Ushr
            | Op::LessThan
            | Op::LessEq
            | Op::GreaterThan
            | Op::GreaterEq
            | Op::Equal
            | Op::NotEqual => {
                ok &= reg(operands, 1)
                    .ok()
                    .is_some_and(|regn| !pending.contains(&regn));
                ok &= reg(operands, 2)
                    .ok()
                    .is_some_and(|regn| !pending.contains(&regn));
            }
            Op::Return | Op::ReturnValue => {
                ok &= reg(operands, 0)
                    .ok()
                    .is_some_and(|regn| !pending.contains(&regn));
            }
            Op::JumpIfFalse | Op::JumpIfTrue => {
                ok &= reg(operands, 1)
                    .ok()
                    .is_some_and(|regn| !pending.contains(&regn));
            }
            Op::StoreUpvalue | Op::StoreUpvalueChecked => {
                ok &= reg(operands, 0)
                    .ok()
                    .is_some_and(|regn| !pending.contains(&regn));
            }
            Op::MakeFunction | Op::MakeClosure if instr.make_self => {}
            Op::LoadUpvalue => {}
            op if is_inline_pure_op(op) => {}
            _ => {
                if std::env::var_os("OTTER_JIT_TRACE").is_some() {
                    eprintln!(
                        "[otter-jit] dead-self skip callee {} pc {} op {:?} make_self={} pending={pending:?}",
                        callee.function_id,
                        instr.byte_pc,
                        instr.op(code_block),
                        instr.make_self,
                    );
                }
                return false;
            }
        }
        if !ok {
            if std::env::var_os("OTTER_JIT_TRACE").is_some() {
                eprintln!(
                    "[otter-jit] dead-self read callee {} pc {} op {:?} pending={pending:?}",
                    callee.function_id,
                    instr.byte_pc,
                    instr.op(code_block),
                );
            }
            return false;
        }

        match instr.op(code_block) {
            Op::LoadInt32
            | Op::LoadNumber
            | Op::LoadUndefined
            | Op::LoadNull
            | Op::LoadHole
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::BitwiseOr
            | Op::BitwiseAnd
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
            | Op::Ushr
            | Op::LessThan
            | Op::LessEq
            | Op::GreaterThan
            | Op::GreaterEq
            | Op::Equal
            | Op::NotEqual
            | Op::ToPrimitive
            | Op::ToNumeric => {
                if let Ok(dst) = reg(operands, 0) {
                    pending.retain(|&seen| seen != dst);
                }
            }
            Op::LoadLocal => {
                let Ok(dst) = reg(operands, 0) else {
                    return false;
                };
                let Ok(src) = local_index(operands, 1) else {
                    return false;
                };
                let src_is_self = pending.contains(&src);
                pending.retain(|&seen| seen != dst);
                if src_is_self {
                    pending.push(dst);
                }
            }
            Op::StoreLocal => {
                let Ok(src) = reg(operands, 0) else {
                    return false;
                };
                let Ok(dst) = local_index(operands, 1) else {
                    return false;
                };
                let src_is_self = pending.contains(&src);
                pending.retain(|&seen| seen != dst);
                if src_is_self {
                    pending.push(dst);
                }
            }
            Op::LoadUpvalue => {
                if let Ok(dst) = reg(operands, 0) {
                    pending.retain(|&seen| seen != dst);
                }
            }
            Op::MakeFunction | Op::MakeClosure if instr.make_self => {
                let Ok(dst) = reg(operands, 0) else {
                    return false;
                };
                pending.retain(|&seen| seen != dst);
                pending.push(dst);
            }
            _ => {}
        }
    }
    true
}

pub(super) fn classify_inline_call(callee: &JitInlineCallee) -> Option<InlineCallKind> {
    let code_block = callee.code_block.as_ref();
    let has_upvalue_op = callee.instructions.iter().any(|instr| {
        matches!(
            instr.op(code_block),
            Op::LoadUpvalue | Op::StoreUpvalue | Op::StoreUpvalueChecked
        )
    });
    if !has_upvalue_op {
        let ops_ok = callee
            .instructions
            .iter()
            .all(|instr| inline_plain_op_allowed(code_block, instr));
        let dead_self = self_bindings_are_dead(callee);
        if std::env::var_os("OTTER_JIT_TRACE").is_some() && (!ops_ok || !dead_self) {
            let bad_op = callee
                .instructions
                .iter()
                .find(|instr| !inline_plain_op_allowed(code_block, instr))
                .map(|instr| (instr.byte_pc, instr.op(code_block)));
            eprintln!(
                "[otter-jit] inline call classify skip callee {}: ops_ok={ops_ok} dead_self={dead_self} bad_op={bad_op:?}",
                callee.function_id
            );
        }
        return (ops_ok && dead_self).then_some(InlineCallKind::Plain);
    }
    if !self_bindings_are_dead(callee) {
        if std::env::var_os("OTTER_JIT_TRACE").is_some() {
            eprintln!(
                "[otter-jit] inline call classify skip callee {}: live self binding",
                callee.function_id
            );
        }
        return None;
    }

    let mut regs = vec![InlineKnown::Unknown; usize::from(callee.register_count)];
    let mut store_seen = false;
    for instr in &callee.instructions {
        let operands = InstructionOperandView {
            code_block,
            instruction: instr,
        };
        let read = |regs: &[InlineKnown], regn: u16| -> Option<InlineKnown> {
            regs.get(regn as usize).copied()
        };
        let write = |regs: &mut [InlineKnown], regn: u16, kind: InlineKnown| -> Option<()> {
            let slot = regs.get_mut(regn as usize)?;
            *slot = kind;
            Some(())
        };

        match instr.op(code_block) {
            Op::LoadInt32 | Op::LoadNumber => {
                write(&mut regs, reg(operands, 0).ok()?, InlineKnown::Number)?;
            }
            Op::LoadTrue | Op::LoadFalse => {
                write(&mut regs, reg(operands, 0).ok()?, InlineKnown::Bool)?;
            }
            Op::LoadUndefined | Op::LoadHole => {
                write(&mut regs, reg(operands, 0).ok()?, InlineKnown::Unknown)?;
            }
            Op::LoadLocal => {
                let dst = reg(operands, 0).ok()?;
                let src = local_index(operands, 1).ok()?;
                let kind = read(&regs, src)?;
                write(&mut regs, dst, kind)?;
            }
            Op::StoreLocal => {
                let src = reg(operands, 0).ok()?;
                let dst = local_index(operands, 1).ok()?;
                let kind = read(&regs, src)?;
                write(&mut regs, dst, kind)?;
            }
            Op::LoadUpvalue => {
                write(&mut regs, reg(operands, 0).ok()?, InlineKnown::Unknown)?;
            }
            Op::ToPrimitive => {
                let dst = reg(operands, 0).ok()?;
                let src = reg(operands, 1).ok()?;
                let kind = read(&regs, src)?;
                if store_seen && kind != InlineKnown::Number {
                    return None;
                }
                write(&mut regs, dst, kind)?;
            }
            Op::ToNumeric => {
                let dst = reg(operands, 0).ok()?;
                let src = reg(operands, 1).ok()?;
                let kind = read(&regs, src)?;
                if store_seen && kind != InlineKnown::Number {
                    return None;
                }
                write(&mut regs, dst, InlineKnown::Number)?;
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem => {
                let dst = reg(operands, 0).ok()?;
                let lhs = read(&regs, reg(operands, 1).ok()?)?;
                let rhs = read(&regs, reg(operands, 2).ok()?)?;
                if store_seen && (lhs != InlineKnown::Number || rhs != InlineKnown::Number) {
                    return None;
                }
                write(&mut regs, dst, InlineKnown::Number)?;
            }
            Op::BitwiseOr
            | Op::BitwiseAnd
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
            | Op::Ushr
            | Op::LessThan
            | Op::LessEq
            | Op::GreaterThan
            | Op::GreaterEq
            | Op::Equal
            | Op::NotEqual => {
                let dst = reg(operands, 0).ok()?;
                let lhs = read(&regs, reg(operands, 1).ok()?)?;
                let rhs = read(&regs, reg(operands, 2).ok()?)?;
                if store_seen {
                    return None;
                }
                let result = if matches!(
                    instr.op(code_block),
                    Op::LessThan
                        | Op::LessEq
                        | Op::GreaterThan
                        | Op::GreaterEq
                        | Op::Equal
                        | Op::NotEqual
                ) {
                    InlineKnown::Bool
                } else {
                    let _ = (lhs, rhs);
                    InlineKnown::Number
                };
                write(&mut regs, dst, result)?;
            }
            Op::StoreUpvalue | Op::StoreUpvalueChecked => {
                let src = reg(operands, 0).ok()?;
                if read(&regs, src)? != InlineKnown::Number {
                    return None;
                }
                store_seen = true;
            }
            Op::Return | Op::ReturnValue | Op::ReturnUndefined => {}
            // Keep upvalue inlining straight-line. The existing plain
            // inliner still owns branchy pure callees.
            _ => return None,
        }
    }
    Some(InlineCallKind::ClosureUpvalues)
}

/// Emit one op of an inlined callee body. The frame-register base `x19`
/// already points at the callee scratch window, so `load_reg`/`store_reg`
/// address callee registers. Bails route to `bail` (the site's scratch-aware
/// bail) without restamping `resume_pc`, so a bail re-runs the whole call in
/// the interpreter. `Return*` leaves the result in `x9` and branches to
/// `inline_done`. Internal branches resolve through `clabels` (one private
/// label per callee logical PC).
pub(super) fn emit_inline_pure_op(
    ops: &mut Assembler,
    code_block: &otter_vm::CodeBlock,
    instr: &otter_vm::JitInstructionMetadata,
    bail: DynamicLabel,
    inline_done: DynamicLabel,
    clabels: &BTreeMap<u32, DynamicLabel>,
    cage_base: usize,
) -> Result<(), Unsupported> {
    let ops_ref = InstructionOperandView {
        code_block,
        instruction: instr,
    };
    let ctarget = |rel: i32| -> Result<DynamicLabel, Unsupported> {
        let t = branch_target(code_block, instr, rel);
        u32::try_from(t)
            .ok()
            .and_then(|pc| clabels.get(&pc).copied())
            .ok_or(Unsupported::BranchTarget(t))
    };
    match instr.op(code_block) {
        Op::LoadInt32 => {
            let dst = reg(ops_ref, 0)?;
            let v = imm32(ops_ref, 1)?;
            emit_load_u64(ops, 9, value_tag::NUMBER_TAG | u64::from(v as u32));
            store_reg(ops, 9, dst)?;
        }
        Op::MakeFunction | Op::MakeClosure if instr.make_self => {}
        Op::LoadNumber => {
            let dst = reg(ops_ref, 0)?;
            let Some(value) = instr.load_number else {
                return Err(Unsupported::OperandShape("load-number constant"));
            };
            // Materialize the boxed `Value` (int32 or offset-double), not the
            // raw f64 bits.
            emit_load_u64(ops, 9, otter_vm::Value::number_f64(value).to_bits());
            store_reg(ops, 9, dst)?;
        }
        Op::LoadLocal => {
            let dst = reg(ops_ref, 0)?;
            let idx = local_index(ops_ref, 1)?;
            load_reg(ops, 9, idx)?;
            store_reg(ops, 9, dst)?;
        }
        Op::LoadUndefined => {
            let dst = reg(ops_ref, 0)?;
            emit_load_u64(ops, 9, VALUE_UNDEFINED);
            store_reg(ops, 9, dst)?;
        }
        Op::LoadHole => {
            let dst = reg(ops_ref, 0)?;
            emit_load_u64(ops, 9, VALUE_HOLE);
            store_reg(ops, 9, dst)?;
        }
        Op::LoadTrue => {
            let dst = reg(ops_ref, 0)?;
            emit_load_u64(ops, 9, VALUE_TRUE);
            store_reg(ops, 9, dst)?;
        }
        Op::LoadFalse => {
            let dst = reg(ops_ref, 0)?;
            emit_load_u64(ops, 9, VALUE_FALSE);
            store_reg(ops, 9, dst)?;
        }
        Op::StoreLocal => {
            let src = reg(ops_ref, 0)?;
            let idx = local_index(ops_ref, 1)?;
            load_reg(ops, 9, src)?;
            store_reg(ops, 9, idx)?;
        }
        Op::LoadUpvalue => {
            if cage_base == 0 {
                return Err(Unsupported::OperandShape("inline upvalue without cage"));
            }
            let dst = reg(ops_ref, 0)?;
            let idx = imm32(ops_ref, 1)?;
            if idx < 0 {
                return Err(Unsupported::OperandShape("upvalue index"));
            }
            let idx_off = u32::try_from(idx)
                .ok()
                .and_then(|idx| idx.checked_mul(UPVALUE_CELL_SIZE))
                .ok_or(Unsupported::OperandShape("upvalue index"))?;
            if idx_off > 32760 {
                return Err(Unsupported::OperandShape("upvalue index"));
            }
            dynasm!(ops
                ; .arch aarch64
                ; ldr x9, [x20, UPVALUES_PTR_OFFSET]
                ; cbz x9, =>bail
                ; ldr w10, [x9, idx_off]
            );
            emit_load_u64(ops, 11, cage_base as u64);
            emit_load_u64(ops, 12, VALUE_HOLE);
            dynasm!(ops
                ; .arch aarch64
                ; add x11, x11, x10
                ; ldr x9, [x11, UPVALUE_VALUE_OFFSET]
                ; cmp x9, x12
                ; b.eq =>bail
            );
            store_reg(ops, 9, dst)?;
        }
        Op::StoreUpvalue | Op::StoreUpvalueChecked => {
            if cage_base == 0 {
                return Err(Unsupported::OperandShape("inline upvalue without cage"));
            }
            let src = reg(ops_ref, 0)?;
            let idx = imm32(ops_ref, 1)?;
            if idx < 0 {
                return Err(Unsupported::OperandShape("upvalue index"));
            }
            let idx_off = u32::try_from(idx)
                .ok()
                .and_then(|idx| idx.checked_mul(UPVALUE_CELL_SIZE))
                .ok_or(Unsupported::OperandShape("upvalue index"))?;
            if idx_off > 32760 {
                return Err(Unsupported::OperandShape("upvalue index"));
            }
            load_reg(ops, 12, src)?;
            dynasm!(ops
                ; .arch aarch64
                ; movz x11, NUMBER_TAG_HI16, lsl #48
                ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                ; tst x12, x11
                ; b.eq =>bail
                ; ldr x9, [x20, UPVALUES_PTR_OFFSET]
                ; cbz x9, =>bail
                ; ldr w10, [x9, idx_off]
            );
            emit_load_u64(ops, 13, cage_base as u64);
            dynasm!(ops ; .arch aarch64 ; add x13, x13, x10);
            if instr.op(code_block) == Op::StoreUpvalueChecked {
                emit_load_u64(ops, 11, VALUE_HOLE);
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x14, [x13, UPVALUE_VALUE_OFFSET]
                    ; cmp x14, x11
                    ; b.eq =>bail
                );
            }
            dynasm!(ops ; .arch aarch64 ; str x12, [x13, UPVALUE_VALUE_OFFSET]);
        }
        Op::Add | Op::Sub | Op::Mul => emit_add_sub_mul(ops, ops_ref, bail, instr.op(code_block))?,
        Op::Div => emit_div(ops, ops_ref, bail)?,
        Op::Rem => emit_rem(ops, ops_ref, bail)?,
        Op::BitwiseOr => emit_int_binop(ops, ops_ref, bail, IntBinOp::Or)?,
        Op::BitwiseAnd => emit_int_binop(ops, ops_ref, bail, IntBinOp::And)?,
        Op::BitwiseXor => emit_int_binop(ops, ops_ref, bail, IntBinOp::Xor)?,
        Op::Shl => emit_int_binop(ops, ops_ref, bail, IntBinOp::Shl)?,
        Op::Shr => emit_int_binop(ops, ops_ref, bail, IntBinOp::Shr)?,
        Op::Ushr => emit_ushr(ops, ops_ref, bail)?,
        Op::LessThan => emit_cmp(ops, ops_ref, bail, Cmp::Lt)?,
        Op::LessEq => emit_cmp(ops, ops_ref, bail, Cmp::Le)?,
        Op::GreaterThan => emit_cmp(ops, ops_ref, bail, Cmp::Gt)?,
        Op::GreaterEq => emit_cmp(ops, ops_ref, bail, Cmp::Ge)?,
        Op::Equal => emit_cmp(ops, ops_ref, bail, Cmp::Eq)?,
        Op::NotEqual => emit_cmp(ops, ops_ref, bail, Cmp::Ne)?,
        Op::ToPrimitive => {
            let dst = reg(ops_ref, 0)?;
            let src = reg(ops_ref, 1)?;
            emit_to_primitive_identity(ops, dst, src, bail)?;
        }
        Op::ToNumeric => {
            let dst = reg(ops_ref, 0)?;
            let src = reg(ops_ref, 1)?;
            load_reg(ops, 9, src)?;
            guard_number!(ops, 9, bail);
            store_reg(ops, 9, dst)?;
        }
        Op::Jump => {
            let rel = imm32(ops_ref, 0)?;
            let tgt = ctarget(rel)?;
            dynasm!(ops ; .arch aarch64 ; b =>tgt);
        }
        Op::JumpIfFalse | Op::JumpIfTrue => {
            let rel = imm32(ops_ref, 0)?;
            let cond = reg(ops_ref, 1)?;
            let tgt = ctarget(rel)?;
            load_reg(ops, 9, cond)?;
            dynasm!(ops
                ; .arch aarch64
                ; sub x14, x9, #(VALUE_FALSE as u32)          // bail unless boolean
                ; cmp x14, #1
                ; b.hi =>bail
                ; cmp x9, #(VALUE_TRUE as u32)                // eq iff true
            );
            if matches!(instr.op(code_block), Op::JumpIfFalse) {
                dynasm!(ops ; .arch aarch64 ; b.ne =>tgt);
            } else {
                dynasm!(ops ; .arch aarch64 ; b.eq =>tgt);
            }
        }
        Op::Return | Op::ReturnValue => {
            let src = reg(ops_ref, 0)?;
            load_reg(ops, 9, src)?;
            dynasm!(ops ; .arch aarch64 ; b =>inline_done);
        }
        Op::ReturnUndefined => {
            emit_load_u64(ops, 9, VALUE_UNDEFINED);
            dynasm!(ops ; .arch aarch64 ; b =>inline_done);
        }
        // Pre-scanned by `is_inline_pure_op`; unreachable in practice.
        _ => return Err(Unsupported::ArgCount(0)),
    }
    Ok(())
}

/// Try to splice `callee`'s body into the current `Op::Call` site instead of
/// emitting the per-call bridge. Returns `Ok(true)` when inlined, `Ok(false)`
/// when the callee fails the pure-leaf / size / arity test (the caller then
/// emits the normal direct-call bridge).
///
/// The body runs only after a guard confirms the callee register holds
/// exactly the speculated closure-less function value. It runs in a fresh
/// native-stack scratch window the frame-register base `x19` is repointed at;
/// `x19` (from the ctx) and `sp` are restored on every exit, including the
/// bail path. Because the body has no GC point and commits nothing
/// observable before a possible bail — and never restamps `resume_pc` — a guard
/// or body bail re-runs the whole call in the interpreter, idempotently.
pub(super) fn try_emit_inline_call(
    ops: &mut Assembler,
    callee: &JitInlineCallee,
    call_operands: impl WordOperands,
    cage_base: usize,
    bail: DynamicLabel,
) -> Result<bool, Unsupported> {
    let dst = reg(call_operands, 0)?;
    let callee_reg = reg(call_operands, 1)?;
    let argc = const_index(call_operands, 2)? as usize;
    let Some(kind) = classify_inline_call(callee) else {
        if std::env::var_os("OTTER_JIT_TRACE").is_some() {
            eprintln!(
                "[otter-jit] inline call skip callee {}: classify",
                callee.function_id
            );
        }
        return Ok(false);
    };

    if argc != usize::from(callee.param_count)
        || argc > INLINE_MAX_ARGS
        || callee.register_count > INLINE_MAX_REGS
        || callee.instructions.len() > INLINE_MAX_INSTRS
        || (kind == InlineCallKind::ClosureUpvalues && cage_base == 0)
    {
        if std::env::var_os("OTTER_JIT_TRACE").is_some() {
            eprintln!(
                "[otter-jit] inline call skip callee {}: shape argc={argc} params={} regs={} instrs={} kind={kind:?} cage_base={}",
                callee.function_id,
                callee.param_count,
                callee.register_count,
                callee.instructions.len(),
                cage_base,
            );
        }
        return Ok(false);
    }

    // One private label per callee logical PC for internal branches.
    let mut clabels: BTreeMap<u32, DynamicLabel> = BTreeMap::new();
    for i in &callee.instructions {
        clabels.insert(
            i.instruction_pc(&callee.code_block),
            ops.new_dynamic_label(),
        );
    }
    let inline_done = ops.new_dynamic_label();
    let inline_bail = ops.new_dynamic_label();
    let after = ops.new_dynamic_label();
    let saved_upvalues_slot = u32::from(callee.register_count);
    let scratch_regs =
        u32::from(callee.register_count) + u32::from(kind == InlineCallKind::ClosureUpvalues);
    let scratch_bytes = (scratch_regs * 8).next_multiple_of(16);

    // Identity guard (x19 = caller frame base, sp not yet moved). Plain
    // function values compare directly. Closure-upvalue inlines ask the VM
    // to validate the current closure's function id and unsupported closure
    // metadata, returning the immutable upvalue-spine base on success.
    if kind == InlineCallKind::Plain {
        load_reg(ops, 9, callee_reg)?;
        emit_load_u64(
            ops,
            10,
            value_tag::FUNCTION_ID_TAG | (u64::from(callee.function_id) << 16),
        );
        dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.ne =>bail);
    } else {
        dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, callee_reg as u32);
        emit_load_u64(ops, 2, u64::from(callee.function_id));
        emit_load_u64(
            ops,
            16,
            jit_inline_closure_upvalues_stub as *const () as u64,
        );
        dynasm!(ops
            ; .arch aarch64
            ; blr x16
            ; cbz x0, =>bail
            ; mov x15, x0
        );
    }

    // Reserve scratch, copy args into param slots (read via caller base x19),
    // zero the remaining slots to undefined (a fresh frame's register state),
    // then repoint x19 at the scratch base for the body.
    if scratch_bytes > 0 {
        dynasm!(ops ; .arch aarch64 ; sub sp, sp, scratch_bytes);
    }
    if kind == InlineCallKind::ClosureUpvalues {
        let saved_off = saved_upvalues_slot * 8;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x14, [x20, UPVALUES_PTR_OFFSET]
            ; str x14, [sp, saved_off]
            ; str x15, [x20, UPVALUES_PTR_OFFSET]
        );
    }
    for slot in 0..argc {
        let areg = reg(call_operands, 3 + slot)?;
        load_reg(ops, 9, areg)?;
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
    }
    emit_load_u64(ops, 9, VALUE_UNDEFINED);
    for slot in argc..usize::from(callee.register_count) {
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
    }
    dynasm!(ops ; .arch aarch64 ; add x19, sp, #0);

    for i in &callee.instructions {
        let instruction_pc = i.instruction_pc(&callee.code_block);
        dynasm!(ops ; .arch aarch64 ; =>clabels[&instruction_pc]);
        emit_inline_pure_op(
            ops,
            &callee.code_block,
            i,
            inline_bail,
            inline_done,
            &clabels,
            cage_base,
        )?;
    }

    // Normal completion: result in x9, unwind scratch, restore caller base,
    // store to dst.
    dynasm!(ops ; .arch aarch64 ; =>inline_done);
    if kind == InlineCallKind::ClosureUpvalues {
        let saved_off = saved_upvalues_slot * 8;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x14, [sp, saved_off]
            ; str x14, [x20, UPVALUES_PTR_OFFSET]
        );
    }
    if scratch_bytes > 0 {
        dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes);
    }
    dynasm!(ops
        ; .arch aarch64
        ; ldr x19, [x20]
    );
    store_reg(ops, 9, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>after);

    // Bail path: unwind scratch so the shared bail epilogue sees the frame
    // base sp (it reloads x19/x20 from the stack), then jump to it.
    dynasm!(ops ; .arch aarch64 ; =>inline_bail);
    if kind == InlineCallKind::ClosureUpvalues {
        let saved_off = saved_upvalues_slot * 8;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x14, [sp, saved_off]
            ; str x14, [x20, UPVALUES_PTR_OFFSET]
        );
    }
    if scratch_bytes > 0 {
        dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes);
    }
    dynasm!(ops ; .arch aarch64 ; b =>bail ; =>after);
    Ok(true)
}

/// Whether an op may appear in an inlined read-only method body: the pure
/// leaf set plus `LoadThis` (reads the spliced receiver slot) and
/// `LoadProperty` (a sealed load from the receiver at a baked offset). Any
/// other op — notably a property/element store — aborts the inline attempt,
/// so a method with a side effect keeps using the full method call.
pub(super) fn is_inline_method_op(op: Op) -> bool {
    is_inline_pure_op(op) || matches!(op, Op::LoadThis | Op::LoadProperty | Op::StoreProperty)
}

/// Ops that cannot bail once emitted, so they are safe to run *after* an
/// inline `StoreProperty` has already mutated the receiver (a bail there
/// would re-run the whole method in the interpreter and double-apply the
/// store). Loads of immediates/locals and `Return*` qualify; anything that
/// can guard-and-bail (property access, arithmetic, coercions) does not.
pub(super) fn is_nonbailing_after_store(op: Op) -> bool {
    matches!(
        op,
        Op::LoadThis
            | Op::LoadInt32
            | Op::LoadLocal
            | Op::LoadUndefined
            | Op::LoadHole
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::StoreLocal
            | Op::Return
            | Op::ReturnValue
            | Op::ReturnUndefined
    )
}

/// Emit one op of an inlined method body. `this_slot` is the scratch slot
/// holding the receiver; `prop_offsets` maps a body `LoadProperty` /
/// `StoreProperty` byte-PC to the baked value slab byte offset.
/// `LoadThis`, `LoadProperty`, and `StoreProperty` are handled here; every
/// other op routes to [`emit_inline_pure_op`].
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_inline_method_op(
    ops: &mut Assembler,
    code_block: &otter_vm::CodeBlock,
    instr: &otter_vm::JitInstructionMetadata,
    this_slot: u16,
    prop_offsets: &rustc_hash::FxHashMap<u32, u32>,
    cage_base: usize,
    recv_shape: u32,
    object_shape_byte: u32,
    object_values_ptr_byte: u32,
    bail: DynamicLabel,
    inline_done: DynamicLabel,
    clabels: &BTreeMap<u32, DynamicLabel>,
) -> Result<(), Unsupported> {
    let ops_ref = InstructionOperandView {
        code_block,
        instruction: instr,
    };
    match instr.op(code_block) {
        Op::LoadThis => {
            let dst = reg(ops_ref, 0)?;
            load_reg(ops, 9, this_slot)?;
            store_reg(ops, 9, dst)?;
            Ok(())
        }
        Op::LoadProperty => {
            let dst = reg(ops_ref, 0)?;
            let obj = reg(ops_ref, 1)?;
            let off = *prop_offsets
                .get(&instr.byte_pc)
                .ok_or(Unsupported::ArgCount(0))?;
            load_reg(ops, 9, obj)?;
            dynasm!(ops
                ; .arch aarch64
                ; movz x11, NUMBER_TAG_HI16, lsl #48
                ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                ; tst x9, x11
                ; b.ne =>bail
                ; mov w12, w9
            );
            emit_load_u64(ops, 13, cage_base as u64);
            dynasm!(ops
                ; .arch aarch64
                ; add x13, x13, x12
                ; ldr x13, [x13, object_values_ptr_byte]
                ; cbz x13, =>bail
                ; ldr w9, [x13, off]                // 4-byte compressed slot
            );
            emit_decompress_slot(ops, cage_base as u64, bail);
            store_reg(ops, 9, dst)?;
            Ok(())
        }
        Op::StoreProperty => {
            // Sealed value-slab store `recv.<prop> = src`. The receiver shape
            // is re-guarded (the baked offset is only valid for it) and the
            // value is required to be a non-`Gc` primitive — a pointer value
            // would need a generational write barrier that cannot run in the
            // remapped scratch window, so it bails *before* writing and the
            // interpreter re-runs the store with the barrier. Every guard
            // here bails ahead of the `str`, so no mutation is lost on a
            // fallback; the site emitter forbids any later bailing op.
            let obj = reg(ops_ref, 0)?;
            let src = reg(ops_ref, 2)?;
            let off = *prop_offsets
                .get(&instr.byte_pc)
                .ok_or(Unsupported::ArgCount(0))?;
            load_reg(ops, 9, obj)?;
            dynasm!(ops
                ; .arch aarch64
                ; movz x11, NUMBER_TAG_HI16, lsl #48
                ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                ; tst x9, x11
                ; b.ne =>bail
                ; mov w12, w9
            );
            emit_load_u64(ops, 13, cage_base as u64);
            dynasm!(ops
                ; .arch aarch64
                ; add x13, x13, x12
                ; ldrb w14, [x13]
                ; cmp w14, OBJECT_BODY_TYPE_TAG
                ; b.ne =>bail
                ; ldr w14, [x13, object_shape_byte]
                ; movz w15, recv_shape & 0xffff
                ; movk w15, (recv_shape >> 16) & 0xffff, lsl #16
                ; cmp w14, w15
                ; b.ne =>bail
            );
            load_reg(ops, 9, src)?;
            dynasm!(ops
                ; .arch aarch64
                // Only a barrier-free primitive is inlined; a heap cell needs
                // the generational write barrier and bails to the interpreter.
                ; movz x11, NUMBER_TAG_HI16, lsl #48
                ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                ; tst x9, x11
                ; b.eq =>bail                          // heap cell → interpreter
                ; ldr x13, [x13, object_values_ptr_byte]
                ; cbz x13, =>bail
            );
            emit_compress_slot_or_bail(ops, bail);
            dynasm!(ops ; .arch aarch64 ; str w10, [x13, off]);
            Ok(())
        }
        _ => emit_inline_pure_op(
            ops,
            code_block,
            instr,
            bail,
            inline_done,
            clabels,
            cage_base,
        ),
    }
}

/// Whether `method`'s baked body can be spliced inline for a call of `argc`
/// arguments. Mirrors the emit-time constraints the inline body relies on:
/// arity match, register/instruction/arg budgets, an all-inlinable op set,
/// and no bailing op after an in-place `StoreProperty` (a post-store bail
/// would re-run the whole method and double-apply the mutation).
pub(super) fn inline_method_emit_eligible(method: &JitInlineMethod, argc: usize) -> bool {
    let code_block = method.code_block.as_ref();
    if argc != usize::from(method.param_count)
        || argc > INLINE_MAX_ARGS
        || method.register_count >= INLINE_MAX_REGS
        || method.instructions.len() > INLINE_MAX_INSTRS
        || !method
            .instructions
            .iter()
            .all(|i| is_inline_method_op(i.op(code_block)))
    {
        return false;
    }
    let mut store_seen = false;
    for i in &method.instructions {
        if store_seen && !is_nonbailing_after_store(i.op(code_block)) {
            return false;
        }
        if i.op(code_block) == Op::StoreProperty {
            store_seen = true;
        }
    }
    true
}

/// Emit one inline method attempt: the inline identity guard followed by the
/// spliced body. On any guard mismatch (receiver tag/shape, prototype
/// tag/shape, method-slot tag, or resolved `function_id`) control branches to
/// `miss` — for a monomorphic site that is the in-place method bridge; for a
/// polymorphic chain it is the next target's guard. On normal completion the
/// result is written to the call's `dst` and control branches to `after`. A
/// body store-bail unwinds the scratch window and branches to the shared
/// `bail`. The caller must have checked [`inline_method_emit_eligible`].
///
/// Soundness: the guard re-reads the receiver shape and re-resolves the
/// prototype method slot every call, so a prototype-method reassignment or a
/// receiver of a different shape lands on `miss` (no stale dispatch). All
/// guards run *before* the scratch window is reserved and *before* any
/// in-place store, so routing `miss` to a sibling attempt mutates no state.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_inline_method_attempt(
    ops: &mut Assembler,
    method: &JitInlineMethod,
    call_operands: impl WordOperands,
    argc: usize,
    cage_base: usize,
    object_shape_byte: u32,
    object_values_ptr_byte: u32,
    jit_proto_byte: u32,
    closure_fid_byte: u32,
    miss: DynamicLabel,
    after: DynamicLabel,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    let dst = reg(call_operands, 0)?;
    let recv_reg = reg(call_operands, 1)?;

    let mut clabels: BTreeMap<u32, DynamicLabel> = BTreeMap::new();
    for i in &method.instructions {
        clabels.insert(
            i.instruction_pc(&method.code_block),
            ops.new_dynamic_label(),
        );
    }
    let inline_done = ops.new_dynamic_label();
    let inline_bail = ops.new_dynamic_label();
    let fid_immediate = ops.new_dynamic_label();
    let fid_compare = ops.new_dynamic_label();
    // One extra slot past the method register window holds `this`.
    let this_slot = method.register_count;
    let scratch_regs = u32::from(method.register_count) + 1;
    let scratch_bytes = (scratch_regs * 8).next_multiple_of(16);

    // Inline identity guard, no per-call resolve bridge. Decompress the
    // receiver (x19 = caller frame base), require its shape to match the
    // baked one, then chase its flat prototype, guard the prototype's shape,
    // read the method slot, and compare the resolved closure's `function_id`
    // to the baked method id. Re-reading the prototype slot every call keeps
    // this sound against prototype-method reassignment: any mismatch (shape,
    // tag, slot tag, or id) lands on `miss`.
    let recv_off = reg_offset(recv_reg)?;
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x19, recv_off]
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9
    );
    emit_load_u64(ops, 13, cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w14, [x13, object_shape_byte]
        ; movz w15, method.recv_shape & 0xffff
        ; movk w15, (method.recv_shape >> 16) & 0xffff, lsl #16
        ; cmp w14, w15
        ; b.ne =>miss
    );
    for &hop_shape in &method.proto_chain {
        dynasm!(ops
            ; .arch aarch64
            // Flat prototype: load the compressed handle, bail on null,
            // then decompress and guard the hopped object's shape. After
            // the final hop x13 holds the method holder's header.
            ; ldr w9, [x13, jit_proto_byte]
            ; cbz w9, =>miss
        );
        emit_load_u64(ops, 12, cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x12, x9
            ; ldrb w14, [x13]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>miss
            ; ldr w14, [x13, object_shape_byte]
            ; movz w15, hop_shape & 0xffff
            ; movk w15, (hop_shape >> 16) & 0xffff, lsl #16
            ; cmp w14, w15
            ; b.ne =>miss
        );
    }
    dynasm!(ops
        ; .arch aarch64
        // Method slot: load the 64-bit Value from the receiver's or
        // prototype's value slab. A resolved method is either a closure-less
        // bytecode reference (function-id immediate, fid in bits [16, 48)) or
        // a closure cell (`JsClosureBody`, fid read from its body). Decode
        // the function id into w14 either way, then compare to the baked id;
        // a number or any non-closure cell misses.
        ; ldr x13, [x13, object_values_ptr_byte]
        ; cbz x13, =>miss
        ; ldr w9, [x13, method.method_value_byte]   // 4-byte compressed slot
    );
    emit_decompress_slot(ops, cage_base as u64, miss);
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; tst x9, x11
        ; b.ne =>miss                 // a number is not a callable method
        ; and x10, x9, #0xffff
        ; cmp x10, #(FUNCTION_ID_TAG as u32)
        ; b.eq =>fid_immediate
        ; mov w12, w9                 // otherwise a cell: low32 = gc offset
    );
    emit_load_u64(ops, 11, cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; add x11, x11, x12
        // Require a closure body (a non-closure cell has a different header
        // tag at this offset), then read `function_id`.
        ; ldrb w14, [x11]
        ; cmp w14, JS_CLOSURE_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w14, [x11, closure_fid_byte]
        ; b =>fid_compare
        ; =>fid_immediate
        ; lsr x14, x9, #16            // function id in bits [16, 48)
        ; =>fid_compare
        ; movz w15, method.method_fid & 0xffff
        ; movk w15, (method.method_fid >> 16) & 0xffff, lsl #16
        ; cmp w14, w15
        ; b.ne =>miss
    );

    // Reserve scratch, copy method args into param slots, the receiver into
    // the `this` slot (all read via caller base x19), zero remaining slots to
    // undefined, then repoint x19 at the scratch base for the body.
    dynasm!(ops ; .arch aarch64 ; sub sp, sp, scratch_bytes);
    for slot in 0..argc {
        let areg = reg(call_operands, 4 + slot)?;
        load_reg(ops, 9, areg)?;
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
    }
    load_reg(ops, 9, recv_reg)?;
    dynasm!(ops ; .arch aarch64 ; str x9, [sp, u32::from(this_slot) * 8]);
    emit_load_u64(ops, 9, VALUE_UNDEFINED);
    for slot in argc..usize::from(method.register_count) {
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, (slot as u32) * 8]);
    }
    dynasm!(ops ; .arch aarch64 ; add x19, sp, #0);

    for i in &method.instructions {
        let instruction_pc = i.instruction_pc(&method.code_block);
        dynasm!(ops ; .arch aarch64 ; =>clabels[&instruction_pc]);
        emit_inline_method_op(
            ops,
            &method.code_block,
            i,
            this_slot,
            &method.prop_offsets,
            cage_base,
            method.recv_shape,
            object_shape_byte,
            object_values_ptr_byte,
            inline_bail,
            inline_done,
            &clabels,
        )?;
    }

    // Normal completion: result in x9, unwind scratch, restore caller base.
    dynasm!(ops ; .arch aarch64 ; =>inline_done);
    dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes);
    dynasm!(ops ; .arch aarch64 ; ldr x19, [x20]);
    store_reg(ops, 9, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>after);

    // Body bail: unwind scratch, then the shared interpreter bail.
    dynasm!(ops ; .arch aarch64 ; =>inline_bail);
    dynasm!(ops ; .arch aarch64 ; add sp, sp, scratch_bytes ; b =>bail);
    Ok(())
}

/// Splice a tiny monomorphic read-only / sealed-write method body into the
/// current `Op::CallMethodValue` site instead of building a callee frame.
/// Returns `Ok(true)` when inlined, `Ok(false)` when the method fails the
/// op-allowlist / size / arity test (the caller then emits the normal
/// method-call bridge). See [`emit_inline_method_attempt`] for the
/// guard/body/soundness details; here a guard miss takes the in-place call.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_emit_inline_method_call(
    ops: &mut Assembler,
    method: &JitInlineMethod,
    call_operands: impl WordOperands,
    site: u64,
    cage_base: usize,
    object_shape_byte: u32,
    object_values_ptr_byte: u32,
    jit_proto_byte: u32,
    closure_fid_byte: u32,
    bail: DynamicLabel,
    threw: DynamicLabel,
) -> Result<bool, Unsupported> {
    let argc = const_index(call_operands, 3)? as usize;
    if cage_base == 0 || !inline_method_emit_eligible(method, argc) {
        return Ok(false);
    }
    let fallback = ops.new_dynamic_label();
    let after = ops.new_dynamic_label();
    emit_inline_method_attempt(
        ops,
        method,
        call_operands,
        argc,
        cage_base,
        object_shape_byte,
        object_values_ptr_byte,
        jit_proto_byte,
        closure_fid_byte,
        fallback,
        after,
        bail,
    )?;
    // Ineligible at run time (method changed / shape mismatch): the full
    // in-place method call, which restores nothing (sp untouched here).
    dynasm!(ops ; .arch aarch64 ; =>fallback);
    emit_method_call(
        ops,
        call_operands,
        site,
        None,
        None,
        None,
        None,
        bail,
        threw,
    )?;
    dynasm!(ops ; .arch aarch64 ; =>after);
    Ok(true)
}

/// Splice a most-frequent-first chain of inline method attempts for a
/// *polymorphic* `Op::CallMethodValue` site. Each attempt guards its own
/// receiver shape + prototype-method identity; a miss falls through to the
/// next attempt, and a receiver matching none of them takes the in-place
/// method bridge. Returns `Ok(false)` (no inline emitted) when no target is
/// emit-eligible, so the caller emits the normal bridge.
///
/// Soundness is identical to the monomorphic path: every attempt's guards run
/// before it reserves a scratch window or performs any in-place store, so a
/// guard miss that routes control to a sibling attempt has mutated nothing.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_emit_poly_inline_method_call(
    ops: &mut Assembler,
    methods: &[JitInlineMethod],
    call_operands: impl WordOperands,
    site: u64,
    cage_base: usize,
    object_shape_byte: u32,
    object_values_ptr_byte: u32,
    jit_proto_byte: u32,
    closure_fid_byte: u32,
    bail: DynamicLabel,
    threw: DynamicLabel,
) -> Result<bool, Unsupported> {
    let argc = const_index(call_operands, 3)? as usize;
    if cage_base == 0 {
        return Ok(false);
    }
    let eligible: Vec<&JitInlineMethod> = methods
        .iter()
        .filter(|m| inline_method_emit_eligible(m, argc))
        .collect();
    if eligible.is_empty() {
        return Ok(false);
    }
    let after = ops.new_dynamic_label();
    let fallback = ops.new_dynamic_label();
    // One entry label per attempt so each attempt's guard miss can branch to
    // the next attempt; the final attempt's miss branches to `fallback`.
    let entries: Vec<DynamicLabel> = (0..eligible.len())
        .map(|_| ops.new_dynamic_label())
        .collect();
    for (i, method) in eligible.iter().enumerate() {
        dynasm!(ops ; .arch aarch64 ; =>entries[i]);
        let miss = if i + 1 < eligible.len() {
            entries[i + 1]
        } else {
            fallback
        };
        emit_inline_method_attempt(
            ops,
            method,
            call_operands,
            argc,
            cage_base,
            object_shape_byte,
            object_values_ptr_byte,
            jit_proto_byte,
            closure_fid_byte,
            miss,
            after,
            bail,
        )?;
    }
    // No guard matched: the full in-place method call (sp untouched here).
    dynasm!(ops ; .arch aarch64 ; =>fallback);
    emit_method_call(
        ops,
        call_operands,
        site,
        None,
        None,
        None,
        None,
        bail,
        threw,
    )?;
    dynasm!(ops ; .arch aarch64 ; =>after);
    Ok(true)
}

/// Copy isolate- and execution-owned fields shared by every nested `JitCtx`.
/// Callee registers, bindings, frame/upvalues, and safepoints are initialized
/// separately by each native calling convention.
pub(super) fn emit_copy_shared_execution_context(ops: &mut Assembler) {
    for off in [
        THREAD_OFFSET,
        NATIVE_FRAME_OFFSET,
        ERROR_SLOT_OFFSET,
        REG_STACK_BASE_OFFSET,
        REG_TOP_PTR_OFFSET,
        SYNC_REENTRY_DEPTH_PTR_OFFSET,
        ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET,
        COLLECTION_METHOD_ICS_OFFSET,
        DIRECT_METHOD_INLINE_OFFSET,
        GC_HEAP_OFFSET,
        INTERRUPT_FLAG_OFFSET,
        BACKEDGE_FUEL_OFFSET,
    ] {
        dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, off] ; str x9, [sp, off]);
    }
    for off in [SYNC_REENTRY_LIMIT_OFFSET, COLLECTION_METHOD_IC_COUNT_OFFSET] {
        dynasm!(ops ; .arch aarch64 ; ldr w9, [x20, off] ; str w9, [sp, off]);
    }
}

/// Emit a self-recursive `Op::Call` inline, with no Rust frame-build bridge:
/// guard the callee is the running closure, reserve a callee window on the
/// interpreter's flat register stack, bind args, build the callee `JitCtx`,
/// and re-enter the function's own entry. A guard miss or a register-stack
/// overflow falls through to the general direct-call bridge (`emit_call`,
/// emitted at `bridge`). The callee's compiled completion writes its value
/// straight to `dst`; a callee bail rebuilds an interpreter frame from the
/// window and runs it to completion ([`jit_self_call_bail_stub`]).
///
/// Only emitted for a frame-index-free function (see [`is_self_call_safe`]):
/// its body uses no stub that addresses registers through
/// `JitCtx.frame_index`, so a frameless callee window is sound. A guard miss
/// (the call is not self-recursive) or a register-stack overflow bails to the
/// interpreter at the call (`bail`), which reconstructs a real frame.
pub(super) fn emit_self_recursive_call(
    ops: &mut Assembler,
    operands: impl WordOperands,
    regcount: u16,
    self_entry: DynamicLabel,
    bail: DynamicLabel,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let dst = reg(operands, 0)?;
    let callee = reg(operands, 1)?;
    let argc = const_index(operands, 2)? as usize;
    if argc > MAX_INLINE_ARGS {
        return Err(Unsupported::ArgCount(argc));
    }
    let rc = u32::from(regcount);
    let done = ops.new_dynamic_label();
    let returned = ops.new_dynamic_label();
    let bailed = ops.new_dynamic_label();
    let fill = ops.new_dynamic_label();
    let fill_done = ops.new_dynamic_label();
    let undef_bits: u64 = VALUE_UNDEFINED;

    // Guard the callee is the running closure (`ctx.self_closure` @ +8).
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x19, callee as u32 * 8]
        ; ldr x10, [x20, #8]
        ; cmp x9, x10
        ; b.ne =>bail
    );
    // Reserve the window: x12 = &reg_top, x11 = old top, x14 = window ptr,
    // x13 = new top. Overflow → bridge.
    dynasm!(ops
        ; .arch aarch64
        ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
        ; ldr x11, [x12]
        ; ldr x9, [x20, REG_STACK_BASE_OFFSET]
        ; add x14, x9, x11, lsl #3
    );
    emit_load_u64(ops, 13, u64::from(rc));
    dynasm!(ops ; .arch aarch64 ; add x13, x11, x13);
    emit_load_u64(ops, 9, Interpreter::jit_reg_stack_cap() as u64);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x13, x9
        ; b.hi =>bail
        ; ldr x17, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
        ; ldr w9, [x17]
        ; ldr w10, [x20, SYNC_REENTRY_LIMIT_OFFSET]
        ; cmp w9, w10
        ; b.hs =>bail
        ; add w9, w9, #1
        ; str w9, [x17]
        ; str x13, [x12]
    );
    // Zero-fill the window to `undefined`.
    emit_load_u64(ops, 10, undef_bits);
    emit_load_u64(ops, 15, u64::from(rc));
    dynasm!(ops
        ; .arch aarch64
        ; movz x9, 0
        ; =>fill
        ; cmp x9, x15
        ; b.hs =>fill_done
        ; str x10, [x14, x9, lsl #3]
        ; add x9, x9, #1
        ; b =>fill
        ; =>fill_done
    );
    // Bind args into the window's leading slots.
    for slot in 0..argc {
        let areg = reg(operands, 3 + slot)?;
        dynasm!(ops
            ; .arch aarch64
            ; ldr x9, [x19, areg as u32 * 8]
            ; str x9, [x14, slot as u32 * 8]
        );
    }
    // Build the callee `JitCtx` on the native stack and re-enter `self_entry`.
    // regs = window; self_closure / upvalues / vm / stack / context /
    // frame_index / error / reg-stack pointers copy from the caller ctx
    // (self-recursion shares them); this = undefined; resume_pc = 0.
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, JIT_CTX_STACK_SIZE
        ; str x14, [sp]
        ; ldr x9, [x20, #8] ; str x9, [sp, #8]
    );
    emit_load_u64(ops, 9, undef_bits);
    dynasm!(ops ; .arch aarch64 ; str x9, [sp, #16] ; str wzr, [sp, RESUME_PC_OFFSET]);
    emit_copy_shared_execution_context(ops);
    for off in [FRAME_INDEX_OFFSET, UPVALUES_PTR_OFFSET] {
        dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, off] ; str x9, [sp, off]);
    }
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, sp
        ; bl =>self_entry
        ; cmp x1, STATUS_BAILED as u32
        ; b.eq =>bailed
        ; add sp, sp, JIT_CTX_STACK_SIZE
        ; cmp x1, STATUS_RETURNED as u32
        ; b.eq =>returned
        ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
        ; ldr x13, [x12]
    );
    emit_load_u64(ops, 9, u64::from(rc));
    dynasm!(ops
        ; .arch aarch64
        ; sub x13, x13, x9
        ; str x13, [x12]
        ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
        ; ldr w13, [x12]
        ; sub w13, w13, #1
        ; str w13, [x12]
        ; b =>threw
    );
    // Returned: pop the window, store the value into `dst`.
    dynasm!(ops
        ; .arch aarch64
        ; =>returned
        ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
        ; ldr x13, [x12]
    );
    emit_load_u64(ops, 9, u64::from(rc));
    dynasm!(ops
        ; .arch aarch64
        ; sub x13, x13, x9
        ; str x13, [x12]
        ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
        ; ldr w13, [x12]
        ; sub w13, w13, #1
        ; str w13, [x12]
    );
    store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    // Bailed: read the callee's resume PC, drop the native ctx, and run the
    // bailed callee to completion through the bail helper (which rebuilds an
    // interpreter frame from the live window and pops it). Helper returns the
    // value in x0 and status in x1.
    dynasm!(ops
        ; .arch aarch64
        ; =>bailed
    ; ldr w2, [sp, RESUME_PC_OFFSET]
        ; add sp, sp, JIT_CTX_STACK_SIZE
        ; mov x0, x20
        ; mov w1, w2
    );
    emit_load_u64(ops, 2, u64::from(rc));
    emit_load_u64(ops, 16, jit_self_call_bail_stub as *const () as u64);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
        ; ldr w13, [x12]
        ; sub w13, w13, #1
        ; str w13, [x12]
        ; cmp x1, STATUS_THREW as u32
        ; b.eq =>threw
    );
    store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}

/// Whether `view`'s body is safe to run as a frameless self-recursive callee:
/// every op either runs inline against the register window (`x19`) or is a
/// `Call` (self-recursive — resolved by the inline guard — or a guard miss
/// that bails) or the self-binding `MakeFunction`. Every allowed op is
/// safepoint-free. A property/element/runtime operation may allocate or
/// re-enter even when it addresses the flat register window, so it needs a
/// published native activation and disqualifies the frameless path.
pub(super) fn is_self_call_safe(view: &JitCompileSnapshot) -> bool {
    let code_block = view.code_block.as_ref();
    view.instructions.iter().all(|instr| {
        is_inline_pure_op(instr.op(code_block))
            || instr.op(code_block) == Op::LoadThis
            || instr.op(code_block) == Op::Call
            || (matches!(instr.op(code_block), Op::MakeFunction | Op::MakeClosure)
                && instr.make_self)
    })
}

/// Probe the VM-published polymorphic direct-method link table and enter a
/// bytecode method through a rooted flat register window.
/// Every guard precedes the window reservation, so a miss falls through to
/// the normal typed method path without observable state.
pub(super) fn emit_direct_method_inline(
    ops: &mut Assembler,
    operands: impl WordOperands,
    site: u64,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
    done: DynamicLabel,
    threw: DynamicLabel,
) -> Result<bool, Unsupported> {
    use otter_vm::jit::JIT_DIRECT_METHOD_WAYS;

    let argc = const_index(operands, 3)? as usize;
    if argc > MAX_METHOD_ARGS || view.cage_base == 0 {
        return Ok(false);
    }
    let dst = reg(operands, 0)?;
    let recv = reg(operands, 1)?;
    let recv_off = reg_offset(recv)?;
    let returned = ops.new_dynamic_label();
    let bailed = ops.new_dynamic_label();
    let direct_threw = ops.new_dynamic_label();
    let hit = ops.new_dynamic_label();
    let table_byte = site
        .saturating_mul(JIT_DIRECT_METHOD_WAYS as u64)
        .saturating_mul(u64::from(DIRECT_METHOD_INLINE_SLOT_SIZE));

    // Common receiver guard. x8 retains the compressed object offset and
    // x7 the first link slot while each way may chase a prototype.
    dynasm!(ops
        ; .arch aarch64
        ; ldr x7, [x20, DIRECT_METHOD_INLINE_OFFSET]
        ; cbz x7, =>miss
    );
    emit_load_u64(ops, 12, table_byte);
    dynasm!(ops
        ; .arch aarch64
        ; add x7, x7, x12
        // Dense ways: first empty entry means whole site has no asm link.
        // Take cold fallback before receiver decoding or the large guard
        // chain, keeping non-eligible sites to one pointer + entry load.
        ; ldr x16, [x7, DIRECT_METHOD_ENTRY_OFFSET]
        ; cbz x16, =>miss
        ; ldr x9, [x19, recv_off]
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w8, w9
    );

    for way in 0..JIT_DIRECT_METHOD_WAYS {
        let next = if way + 1 == JIT_DIRECT_METHOD_WAYS {
            miss
        } else {
            ops.new_dynamic_label()
        };
        let way_byte = way as u32 * DIRECT_METHOD_INLINE_SLOT_SIZE;
        dynasm!(ops
            ; .arch aarch64
            ; add x17, x7, way_byte
            ; ldr x16, [x17, DIRECT_METHOD_ENTRY_OFFSET]
            // Ways are appended densely and cleared as a whole. An empty
            // entry therefore terminates the chain; no later way can hit.
            ; cbz x16, =>miss
        );
        emit_load_u64(ops, 12, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x12, x8
            ; ldrb w14, [x13]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>next
            ; ldr w14, [x13, view.object_shape_byte]
            ; ldr w15, [x17, DIRECT_METHOD_RECV_SHAPE_OFFSET]
            ; cmp w14, w15
            ; b.ne =>next
            ; ldr w15, [x17, DIRECT_METHOD_ON_RECEIVER_OFFSET]
            ; cbnz w15, >holder
            ; ldr w9, [x13, view.jit_proto_byte]
            ; cbz w9, =>next
        );
        emit_load_u64(ops, 12, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x12, x9
            ; ldrb w14, [x13]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>next
            ; ldr w14, [x13, view.object_shape_byte]
            ; ldr w15, [x17, DIRECT_METHOD_PROTO_SHAPE_OFFSET]
            ; cmp w14, w15
            ; b.ne =>next
            ; holder:
        );
        emit_slab_base(ops, view, 13, 14);
        dynasm!(ops
            ; .arch aarch64
            ; ldr w12, [x17, DIRECT_METHOD_VALUE_BYTE_OFFSET]
            ; ldr w9, [x13, x12]
        );
        emit_decompress_slot(ops, view.cage_base as u64, next);

        let immediate = ops.new_dynamic_label();
        let compare = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; movz x11, NUMBER_TAG_HI16, lsl #48
            ; tst x9, x11
            ; b.ne =>next
            ; and x10, x9, #0xffff
            ; cmp x10, #(FUNCTION_ID_TAG as u32)
            ; b.eq =>immediate
            ; mov w12, w9
        );
        emit_load_u64(ops, 11, view.cage_base as u64);
        dynasm!(ops
            ; .arch aarch64
            ; add x11, x11, x12
            ; ldrb w14, [x11]
            ; cmp w14, JS_CLOSURE_BODY_TYPE_TAG
            ; b.ne =>next
            ; ldr w14, [x11, view.closure_fid_byte]
            ; ldr x10, [x11, view.closure_upvalues_ptr_byte]
            ; b =>compare
            ; =>immediate
            ; lsr x14, x9, #16
            ; movz x10, #0
            ; =>compare
            ; ldr w15, [x17, DIRECT_METHOD_FID_OFFSET]
            ; cmp w14, w15
            ; b.eq =>hit
        );
        if way + 1 != JIT_DIRECT_METHOD_WAYS {
            dynasm!(ops ; .arch aarch64 ; =>next);
        }
    }

    // x17 = selected link, x9 = live method SELF, x10 = live captured
    // upvalue spine. Keep those plus entry/window size in a native metadata
    // record while the callee context occupies the stack below it.
    dynasm!(ops
        ; .arch aarch64
        ; =>hit
        ; ldr x16, [x17, DIRECT_METHOD_ENTRY_OFFSET]
        ; ldr w15, [x17, DIRECT_METHOD_REGISTER_COUNT_OFFSET]
        ; sub sp, sp, #32
        ; str x16, [sp]
        ; str x15, [sp, #8]
        ; str x9, [sp, #16]
        ; str x10, [sp, #24]
        ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
        ; ldr x11, [x12]
        ; ldr x9, [x20, REG_STACK_BASE_OFFSET]
        ; add x14, x9, x11, lsl #3
        ; add x13, x11, x15
    );
    emit_load_u64(ops, 9, Interpreter::jit_reg_stack_cap() as u64);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x13, x9
        ; b.hi >overflow
        ; ldr x17, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
        ; ldr w9, [x17]
        ; ldr w10, [x20, SYNC_REENTRY_LIMIT_OFFSET]
        ; cmp w9, w10
        ; b.hs >overflow
        ; add w9, w9, #1
        ; str w9, [x17]
        ; str x13, [x12]
    );
    emit_load_u64(ops, 10, VALUE_UNDEFINED);
    let fill = ops.new_dynamic_label();
    let fill_done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x9, #0
        ; =>fill
        ; cmp x9, x15
        ; b.hs =>fill_done
        ; str x10, [x14, x9, lsl #3]
        ; add x9, x9, #1
        ; b =>fill
        ; =>fill_done

        // Bind supplied arguments directly into the callee window. A
        // frameless link is restricted to bodies without `arguments`, so
        // slots beyond the formal/register window are semantically dead;
        // missing slots remain the undefined values written above.
    );
    for slot in 0..argc {
        let arg = reg(operands, 4 + slot)?;
        let skip_arg = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; cmp x15, slot as u32
            ; b.ls =>skip_arg
            ; ldr x9, [x19, arg as u32 * 8]
            ; str x9, [x14, slot as u32 * 8]
            ; =>skip_arg
        );
    }
    dynasm!(ops
        ; .arch aarch64

        ; sub sp, sp, JIT_CTX_STACK_SIZE
        ; str x14, [sp]
        ; ldr x9, [sp, JIT_CTX_STACK_SIZE + 16]
        ; str x9, [sp, #8]
        ; ldr x9, [x19, recv_off]
        ; str x9, [sp, #16]
    ; str wzr, [sp, RESUME_PC_OFFSET]
    );
    emit_copy_shared_execution_context(ops);
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x20, FRAME_INDEX_OFFSET]
        ; str x9, [sp, FRAME_INDEX_OFFSET]
    );
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [sp, JIT_CTX_STACK_SIZE + 24]
        ; str x9, [sp, UPVALUES_PTR_OFFSET]
        ; str xzr, [sp, DIRECT_ENTRY_OFFSET]
        ; str xzr, [sp, DIRECT_REGS_OFFSET]
        ; str xzr, [sp, DIRECT_SELF_OFFSET]
        ; str xzr, [sp, DIRECT_THIS_OFFSET]
        ; str xzr, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; str xzr, [sp, DIRECT_UPVALUES_OFFSET]
        ; mov x0, sp
        ; ldr x16, [sp, JIT_CTX_STACK_SIZE]
        ; blr x16
        ; cmp x1, STATUS_RETURNED as u32
        ; b.eq =>returned
        ; cmp x1, STATUS_BAILED as u32
        ; b.eq =>bailed
        ; b =>direct_threw

        ; =>returned
        ; add sp, sp, JIT_CTX_STACK_SIZE
        ; ldr x15, [sp, #8]
        ; add sp, sp, #32
        ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
        ; ldr x13, [x12]
        ; sub x13, x13, x15
        ; str x13, [x12]
        ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
        ; ldr w13, [x12]
        ; sub w13, w13, #1
        ; str w13, [x12]
    );
    store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);

    dynasm!(ops
        ; .arch aarch64
        ; =>direct_threw
        ; add sp, sp, JIT_CTX_STACK_SIZE
        ; ldr x15, [sp, #8]
        ; add sp, sp, #32
        ; ldr x12, [x20, REG_TOP_PTR_OFFSET]
        ; ldr x13, [x12]
        ; sub x13, x13, x15
        ; str x13, [x12]
        ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
        ; ldr w13, [x12]
        ; sub w13, w13, #1
        ; str w13, [x12]
        ; b =>threw

        ; =>bailed
    ; ldr w1, [sp, RESUME_PC_OFFSET]
        ; add sp, sp, JIT_CTX_STACK_SIZE
        ; ldr x2, [sp, #8]
        ; ldr x3, [sp, #16]
        ; ldr x4, [x19, recv_off]
        ; add sp, sp, #32
        ; mov x0, x20
    );
    emit_load_u64(
        ops,
        16,
        jit_direct_method_call_bail_stub as *const () as u64,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; ldr x12, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
        ; ldr w13, [x12]
        ; sub w13, w13, #1
        ; str w13, [x12]
        ; cmp x1, STATUS_THREW as u32
        ; b.eq =>threw
    );
    store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; overflow: ; add sp, sp, #32 ; b =>miss);
    Ok(true)
}

/// Emit a direct `Call`: ask the VM to publish an eligible callee frame,
/// build the callee `JitCtx` on the native stack, branch to the compiled
/// entry, then finish/pop/store through the narrow direct-call ABI. Cold or
/// ineligible calls bail to the interpreter instead of using the generic
/// runtime call bridge.
pub(super) fn emit_call(
    ops: &mut Assembler,
    _operands: impl WordOperands,
    bail: DynamicLabel,
    _threw: DynamicLabel,
) -> Result<(), Unsupported> {
    // The former direct-call ABI asked the interpreter to materialize a
    // HoltStack frame, then re-entered native code. That is neither a
    // native calling convention nor a useful boundary: plain calls bail
    // until they have a frameless native link.
    dynasm!(ops ; .arch aarch64 ; b =>bail);
    Ok(())
}

/// Shared direct-call dispatch tail used after a prepare stub returned
/// status 0 (callee frame published in `ctx.direct_*`). Builds the callee
/// `JitCtx` on the native stack, branches to the compiled entry, and runs
/// the returned / bailed / threw finish helpers, landing at `done`.
///
/// Both the baseline and the optimizing emitter enter compiled callees
/// through this one tail, so the callee `JitCtx` is constructed from a
/// single source: the isolate-boundary fields (`gc_heap`, safepoint table,
/// collection ICs, array-index protector) propagate from the caller ctx and
/// the per-call `direct_*` fields are copied verbatim. A second, hand-copied
/// tail in either tier would be free to drift on which fields it initializes
/// — the drift that left optimizing callees reading uninitialized safepoint
/// and heap slots — so there is deliberately only this one.
pub(crate) fn emit_direct_call_tail(
    ops: &mut Assembler,
    dst: u16,
    threw: DynamicLabel,
    done: DynamicLabel,
) {
    let direct_returned = ops.new_dynamic_label();
    let direct_bailed = ops.new_dynamic_label();
    let direct_threw = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; sub sp, sp, JIT_CTX_STACK_SIZE
        ; ldr x9, [x20, DIRECT_REGS_OFFSET]
        ; str x9, [sp]
        ; ldr x9, [x20, DIRECT_SELF_OFFSET]
        ; str x9, [sp, #8]
        ; ldr x9, [x20, DIRECT_THIS_OFFSET]
        ; str x9, [sp, #16]
    ; str wzr, [sp, RESUME_PC_OFFSET]
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x9, [sp, THREAD_OFFSET]
        ; ldr x9, [x20, NATIVE_FRAME_OFFSET]
        ; str x9, [sp, NATIVE_FRAME_OFFSET]
        ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; str x9, [sp, FRAME_INDEX_OFFSET]
        ; ldr x9, [x20, ERROR_SLOT_OFFSET]
        ; str x9, [sp, ERROR_SLOT_OFFSET]
        // Copy the prepared callee upvalue-spine base so inline upvalue ops
        // in the direct callee read its cells without the stub.
        ; ldr x9, [x20, DIRECT_UPVALUES_OFFSET]
        ; str x9, [sp, UPVALUES_PTR_OFFSET]
        // Propagate the flat register-stack pointers so the direct callee can
        // build its own self-recursive call windows inline.
        ; ldr x9, [x20, REG_STACK_BASE_OFFSET]
        ; str x9, [sp, REG_STACK_BASE_OFFSET]
        ; ldr x9, [x20, REG_TOP_PTR_OFFSET]
        ; str x9, [sp, REG_TOP_PTR_OFFSET]
        ; ldr x9, [x20, SYNC_REENTRY_DEPTH_PTR_OFFSET]
        ; str x9, [sp, SYNC_REENTRY_DEPTH_PTR_OFFSET]
        ; ldr w9, [x20, SYNC_REENTRY_LIMIT_OFFSET]
        ; str w9, [sp, SYNC_REENTRY_LIMIT_OFFSET]
        ; ldr x9, [x20, ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET]
        ; str x9, [sp, ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET]
        ; ldr x9, [x20, COLLECTION_METHOD_ICS_OFFSET]
        ; str x9, [sp, COLLECTION_METHOD_ICS_OFFSET]
        ; ldr w9, [x20, COLLECTION_METHOD_IC_COUNT_OFFSET]
        ; str w9, [sp, COLLECTION_METHOD_IC_COUNT_OFFSET]
        // Propagate the direct-method inline-link table base so a direct
        // callee can itself take the bridge-free method-call fast path.
        ; ldr x9, [x20, DIRECT_METHOD_INLINE_OFFSET]
        ; str x9, [sp, DIRECT_METHOD_INLINE_OFFSET]
        ; ldr x9, [x20, GC_HEAP_OFFSET]
        ; str x9, [sp, GC_HEAP_OFFSET]
        ; ldr x9, [x20, INTERRUPT_FLAG_OFFSET]
        ; str x9, [sp, INTERRUPT_FLAG_OFFSET]
        ; ldr x9, [x20, BACKEDGE_FUEL_OFFSET]
        ; str x9, [sp, BACKEDGE_FUEL_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 16, jit_push_native_activation_stub as *const () as u64);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
        ; mov x0, sp
        ; ldr x16, [x20, DIRECT_ENTRY_OFFSET]
        ; blr x16
        ; cmp x1, STATUS_RETURNED as u32
        ; b.eq =>direct_returned
        ; cmp x1, STATUS_BAILED as u32
        ; b.eq =>direct_bailed
        ; b =>direct_threw
        ; =>direct_returned
        ; str x0, [sp, DIRECT_ENTRY_OFFSET]
        ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; str x9, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 16, jit_pop_native_activation_stub as *const () as u64);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; ldr x2, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; ldr x3, [sp, DIRECT_ENTRY_OFFSET]
        ; add sp, sp, JIT_CTX_STACK_SIZE
        ; mov x0, x20
        ; movz x1, dst as u32
    );
    emit_call_stub(
        ops,
        jit_finish_direct_call_returned_stub as *const () as usize,
        threw,
    );
    dynasm!(ops ; .arch aarch64 ; b =>done);

    dynasm!(ops
        ; .arch aarch64
        ; =>direct_bailed
    ; ldr w9, [sp, RESUME_PC_OFFSET]
        ; str w9, [sp, DIRECT_ENTRY_OFFSET]
        ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; str x9, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 16, jit_pop_native_activation_stub as *const () as u64);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; ldr x2, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; ldr w3, [sp, DIRECT_ENTRY_OFFSET]
        ; add sp, sp, JIT_CTX_STACK_SIZE
        ; mov x0, x20
        ; movz x1, dst as u32
    );
    emit_call_stub(
        ops,
        jit_finish_direct_call_bailed_stub as *const () as usize,
        threw,
    );
    dynasm!(ops ; .arch aarch64 ; b =>done);

    dynasm!(ops
        ; .arch aarch64
        ; =>direct_threw
        ; ldr x9, [x20, DIRECT_FRAME_INDEX_OFFSET]
        ; str x9, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; mov x0, sp
    );
    emit_load_u64(ops, 16, jit_pop_native_activation_stub as *const () as u64);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; ldr x1, [sp, DIRECT_FRAME_INDEX_OFFSET]
        ; add sp, sp, JIT_CTX_STACK_SIZE
        ; mov x0, x20
    );
    emit_call_stub(ops, jit_abort_direct_call_stub as *const () as usize, threw);
    // The caller places `done` (once) after any trailing fallback code.
    dynasm!(ops ; .arch aarch64 ; b =>threw);
}

/// Emit the reusable baseline ABI call sequence for
/// `leaf_no_alloc_stub2_trampoline_pair`.
///
/// Inputs are the current `JitCtx` in `x20`, frame register window in
/// `x19`, and a previously resolved nonzero `RuntimeStubId` in
/// `stub_id_x`. The helper reads the opaque GC heap pointer from `JitCtx`,
/// passes raw boxed receiver/key bits from the frame window, writes `dst`
/// on `Ok`, and branches to `miss` for every non-`Ok` status.
pub(super) fn emit_leaf_no_alloc_stub2_pair_call(
    ops: &mut Assembler,
    stub_id_x: u8,
    dst: u16,
    recv: u16,
    key: Option<u16>,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x0, [x20, GC_HEAP_OFFSET]
        ; mov x1, X(stub_id_x)
    );
    load_reg(ops, 2, recv)?;
    if let Some(key) = key {
        load_reg(ops, 3, key)?;
    } else {
        emit_load_u64(ops, 3, VALUE_UNDEFINED);
    }
    emit_load_u64(
        ops,
        16,
        leaf_no_alloc_stub2_trampoline_pair as *const () as u64,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
        ; cbnz x1, =>miss
    );
    store_reg(ops, 0, dst)
}

pub(super) fn emit_collection_leaf_method_guarded_call(
    ops: &mut Assembler,
    operands: impl WordOperands,
    leaf: &JitCollectionLeafMethod,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    if view.cage_base == 0 {
        return Ok(false);
    }

    let dst = reg(operands, 0)?;
    let recv = reg(operands, 1)?;
    let argc = const_index(operands, 3)? as usize;
    let key = if argc == 0 {
        None
    } else {
        Some(reg(operands, 4)?)
    };
    let guard_flags_byte = view.collection_layout.guard_flags_byte;
    let object_shape_byte = view.object_shape_byte;
    let object_values_ptr_byte = view.object_values_ptr_byte;
    let native_static_fn_byte = view.native_static_fn_byte;
    let method_value_byte = leaf.method_value_byte;
    let receiver_type_tag = u32::from(leaf.receiver_type_tag);
    let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);

    load_reg(ops, 9, recv)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9
    );
    emit_load_u64(ops, 13, view.cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, receiver_type_tag
        ; b.ne =>miss
        ; ldr w14, [x13, guard_flags_byte]
        ; cbnz w14, =>miss
    );

    emit_load_u64(ops, 15, view.cage_base as u64);
    emit_load_u64(ops, 12, u64::from(leaf.proto_offset));
    dynasm!(ops
        ; .arch aarch64
        ; add x15, x15, x12
        ; ldrb w14, [x15]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w14, [x15, object_shape_byte]
    );
    emit_load_u64(ops, 12, u64::from(leaf.proto_shape));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w14, w12
        ; b.ne =>miss
        ; ldr x15, [x15, object_values_ptr_byte]
        ; cbz x15, =>miss
        ; ldr w17, [x15, method_value_byte]
    );
    emit_decompress_slot(ops, view.cage_base as u64, miss);
    dynasm!(ops
        ; .arch aarch64
        ; mov x9, x17
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9
    );
    emit_load_u64(ops, 13, view.cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, native_function_type_tag
        ; b.ne =>miss
        ; ldr x14, [x13, native_static_fn_byte]
    );
    emit_load_u64(ops, 15, leaf.builtin_fn_addr as u64);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x14, x15
        ; b.ne =>miss
    );
    emit_load_u64(ops, 11, u64::from(leaf.leaf_stub_id));
    emit_leaf_no_alloc_stub2_pair_call(ops, 11, dst, recv, key, miss)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(true)
}

/// Emit the shared receiver + prototype-builtin guard for an inline
/// dense-array method. Leaves the dense-array body pointer in `x13`; any
/// guard failure branches to `miss`. The receiver must be a pointer-tagged
/// ordinary dense `Array` (array type tag, no exotic sidecar) and
/// `%Array.prototype%` must still carry the original builtin at the cached
/// shape + slot, so the resolved method can only be that builtin. The body
/// pointer is recomputed from the rooted receiver slot at the end (the
/// prototype guard clobbers `x13`); nothing on this path can move the heap.
pub(super) fn emit_array_dense_proto_guard(
    ops: &mut Assembler,
    recv: u16,
    am: &JitArrayMethod,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let cage_base = view.cage_base as u64;
    let array_tag = u32::from(view.ta_layout.array_type_tag);
    let exotic_byte = view.ta_layout.array_exotic_byte;
    let object_shape_byte = view.object_shape_byte;
    let object_values_ptr_byte = view.object_values_ptr_byte;
    let native_static_fn_byte = view.native_static_fn_byte;
    let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);
    let method_value_byte = am.method_value_byte;

    load_reg(ops, 9, recv)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9
    );
    emit_load_u64(ops, 13, cage_base);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, array_tag
        ; b.ne =>miss
        ; ldr x14, [x13, exotic_byte]
        ; cbnz x14, =>miss
    );

    emit_load_u64(ops, 15, cage_base);
    emit_load_u64(ops, 12, u64::from(am.proto_offset));
    dynasm!(ops
        ; .arch aarch64
        ; add x15, x15, x12
        ; ldrb w14, [x15]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w14, [x15, object_shape_byte]
    );
    emit_load_u64(ops, 12, u64::from(am.proto_shape));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w14, w12
        ; b.ne =>miss
        ; ldr x15, [x15, object_values_ptr_byte]
        ; cbz x15, =>miss
        // The value slab holds 4-byte compressed slots, so the method value is
        // a 32-bit load (the byte offset is `slot * 4` and need not be
        // 8-aligned). The method is expected to be a cell (a native function
        // object): its low-3 tag is `000` and its zero-extended offset is the
        // bare cage offset. Any non-cell (smi / immediate / function id / boxed
        // number) or the empty slot misses to the runtime method bridge.
        ; ldr w9, [x15, method_value_byte]
        ; ands w11, w9, #0x7
        ; b.ne =>miss
        ; cbz w9, =>miss
        ; mov w12, w9
    );
    emit_load_u64(ops, 13, cage_base);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, native_function_type_tag
        ; b.ne =>miss
        ; ldr x14, [x13, native_static_fn_byte]
    );
    emit_load_u64(ops, 15, am.builtin_fn_addr as u64);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x14, x15
        ; b.ne =>miss
    );

    // Recompute the dense-array body pointer into x13 (the prototype guard
    // clobbered it). The receiver tag is already verified.
    load_reg(ops, 9, recv)?;
    dynasm!(ops ; .arch aarch64 ; mov w12, w9);
    emit_load_u64(ops, 13, cage_base);
    dynasm!(ops ; .arch aarch64 ; add x13, x13, x12);
    Ok(())
}

/// Splice an inline `Array.prototype.pop` fast path under the shared
/// dense-array guard. On a hit it removes and returns the last dense element
/// with no call or allocation; on any guard miss it branches to `miss` (the
/// caller continues to the runtime method bridge) and on a hit it branches to
/// `done` (past the bridge). Returns `Ok(false)` (nothing emitted) when the
/// site can't be served inline: no baked cage base, or `pop` called with
/// arguments (only the canonical zero-arg form is modeled).
///
/// GC: the only mutation is shrinking the dense `Vec` length, so the dropped
/// slot falls outside the traced `[0, len)` range and the returned value is
/// rooted in the destination frame slot. No write barrier or safepoint.
pub(super) fn emit_array_pop_inline(
    ops: &mut Assembler,
    operands: impl WordOperands,
    am: &JitArrayMethod,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    if view.cage_base == 0 {
        return Ok(false);
    }
    let dst = reg(operands, 0)?;
    let recv = reg(operands, 1)?;
    let argc = const_index(operands, 3)? as usize;
    if argc != 0 {
        return Ok(false);
    }
    let length_byte = view.ta_layout.array_length_byte;
    let (ptr_word, len_word) = vec_layout_offsets();
    let arr_ptr_byte = view.ta_layout.array_elements_byte + ptr_word;
    let arr_len_byte = view.ta_layout.array_elements_byte + len_word;
    let undef = VALUE_UNDEFINED;

    emit_array_dense_proto_guard(ops, recv, am, view, miss)?;

    // pop body: require the dense invariant (Vec length == logical length);
    // an empty array returns undefined without mutating, otherwise drop and
    // return the last slot.
    let empty = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [x13, arr_len_byte]
        ; ldr x11, [x13, length_byte]
        ; cmp x10, x11
        ; b.ne =>miss
        ; cbz x10, =>empty
        ; sub x10, x10, #1
        ; ldr x12, [x13, arr_ptr_byte]
        ; lsl x15, x10, #3
        ; add x12, x12, x15
        ; ldr x14, [x12]
        ; str x10, [x13, arr_len_byte]
        ; str x10, [x13, length_byte]
    );
    store_reg(ops, 14, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>empty);
    emit_load_u64(ops, 14, undef);
    store_reg(ops, 14, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(true)
}

/// Splice an inline `Array.prototype.push(x)` fast path under the shared
/// dense-array guard. The fast path serves the single-argument, has-spare-
/// capacity case: it writes the value into the next dense slot, bumps the Vec
/// and logical lengths, returns the new length, and marks the receiver's card
/// when the value is a heap pointer (old→young barrier, mirroring the inline
/// dense `StoreElement`). Growth (length == capacity), multi-argument pushes,
/// and any guard miss branch to `miss`, where the runtime method bridge owns
/// the spec-correct reallocation and rooting. A hit branches to `done`.
///
/// Returns `Ok(false)` (nothing emitted) when the site can't be served
/// inline: no baked cage base, or `push` with other than one argument.
pub(super) fn emit_array_push_inline(
    ops: &mut Assembler,
    operands: impl WordOperands,
    am: &JitArrayMethod,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
    done: DynamicLabel,
    threw: DynamicLabel,
) -> Result<bool, Unsupported> {
    if view.cage_base == 0 {
        return Ok(false);
    }
    let dst = reg(operands, 0)?;
    let recv = reg(operands, 1)?;
    let argc = const_index(operands, 3)? as usize;
    if argc != 1 {
        return Ok(false);
    }
    let value = reg(operands, 4)?;
    let length_byte = view.ta_layout.array_length_byte;
    let (ptr_word, len_word) = vec_layout_offsets();
    let arr_ptr_byte = view.ta_layout.array_elements_byte + ptr_word;
    let arr_len_byte = view.ta_layout.array_elements_byte + len_word;
    // The third Vec machine word is the capacity (the std `Vec` is three
    // words: data pointer, capacity, length).
    let cap_word = 24 - ptr_word - len_word;
    let arr_cap_byte = view.ta_layout.array_elements_byte + cap_word;

    emit_array_dense_proto_guard(ops, recv, am, view, miss)?;

    // push body: require the dense invariant and spare capacity; bound the
    // new length to the int32 fast path; an indexed accessor/proto hazard
    // (protector tripped) misses so the bridge applies the spec semantics.
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [x13, arr_len_byte]     // veclen
        ; ldr x11, [x13, length_byte]      // logical length
        ; cmp x10, x11
        ; b.ne =>miss
        ; ldr x14, [x13, arr_cap_byte]     // capacity
        ; cmp x10, x14
        ; b.hs =>miss                      // no spare capacity → bridge grows
        ; add x11, x10, #1                 // new length
    );
    emit_load_u64(ops, 14, i32::MAX as u64);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x11, x14
        ; b.hi =>miss                      // new length out of int32 fast path
        ; ldr x14, [x20, ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET]
        ; ldrb w14, [x14]
        ; cbnz w14, =>miss                 // indexed proto/accessor hazard
        ; ldr x12, [x13, arr_ptr_byte]     // elements Vec data pointer
        ; lsl x15, x10, #3
        ; add x12, x12, x15                // &elements[veclen]
    );
    load_reg(ops, 9, value)?;
    dynasm!(ops
        ; .arch aarch64
        ; str x9, [x12]                    // store value into the new slot
        ; str x11, [x13, arr_len_byte]     // Vec length++
        ; str x11, [x13, length_byte]      // logical length++
        ; movz x14, NUMBER_TAG_HI16, lsl #48
        ; orr x14, x11, x14                // box new length as int32
    );
    store_reg(ops, 14, dst)?;
    // Old→young card barrier when the stored value is a heap pointer,
    // matching the inline dense `StoreElement`. Primitives skip it.
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>done
        ; mov x0, x20
        ; movz x1, recv as u32
        ; movz x2, value as u32
    );
    emit_call_stub(ops, jit_write_barrier_stub as *const () as usize, threw);
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(true)
}

pub(super) fn emit_live_collection_leaf_method_guarded_call(
    ops: &mut Assembler,
    operands: impl WordOperands,
    site: u64,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    if view.cage_base == 0 {
        return Ok(false);
    }

    let dst = reg(operands, 0)?;
    let recv = reg(operands, 1)?;
    let argc = const_index(operands, 3)? as usize;
    let key = if argc == 0 {
        None
    } else {
        Some(reg(operands, 4)?)
    };
    let guard_flags_byte = view.collection_layout.guard_flags_byte;
    let object_shape_byte = view.object_shape_byte;
    let object_values_ptr_byte = view.object_values_ptr_byte;
    let native_static_fn_byte = view.native_static_fn_byte;
    let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);

    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x20, COLLECTION_METHOD_ICS_OFFSET]
        ; cbz x17, =>miss
        ; ldr w10, [x20, COLLECTION_METHOD_IC_COUNT_OFFSET]
    );
    emit_load_u64(ops, 11, site);
    dynasm!(ops ; .arch aarch64 ; cmp x11, x10 ; b.hs =>miss);
    emit_load_u64(
        ops,
        12,
        site.saturating_mul(u64::from(COLLECTION_METHOD_IC_SLOT_SIZE)),
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x17, x17, x12
        ; ldrb w10, [x17, COLLECTION_METHOD_IC_STATE_OFFSET]
        ; cmp w10, JIT_COLLECTION_METHOD_IC_COLLECTION as u32
        ; b.ne =>miss
        ; ldr w11, [x17, COLLECTION_METHOD_IC_LEAF_STUB_ID_OFFSET]
    );
    emit_load_u64(ops, 12, u64::from(JIT_COLLECTION_METHOD_IC_NO_STUB));
    dynasm!(ops ; .arch aarch64 ; cmp x11, x12 ; b.eq =>miss);

    load_reg(ops, 9, recv)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9
    );
    emit_load_u64(ops, 13, view.cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; ldrb w15, [x17, COLLECTION_METHOD_IC_RECEIVER_TYPE_TAG_OFFSET]
        ; cmp w14, w15
        ; b.ne =>miss
        ; ldr w14, [x13, guard_flags_byte]
        ; cbnz w14, =>miss
    );

    emit_load_u64(ops, 15, view.cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; ldr w12, [x17, COLLECTION_METHOD_IC_PROTO_OFFSET]
        ; add x15, x15, x12
        ; ldrb w14, [x15]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w14, [x15, object_shape_byte]
        ; ldr w12, [x17, COLLECTION_METHOD_IC_PROTO_SHAPE_OFFSET]
        ; cmp w14, w12
        ; b.ne =>miss
        ; ldr x15, [x15, object_values_ptr_byte]
        ; cbz x15, =>miss
        ; ldr w12, [x17, COLLECTION_METHOD_IC_METHOD_VALUE_BYTE_OFFSET]
        ; ldr x9, [x15, x12]
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9
    );
    emit_load_u64(ops, 13, view.cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, native_function_type_tag
        ; b.ne =>miss
        ; ldr x14, [x13, native_static_fn_byte]
        ; ldr x15, [x17, COLLECTION_METHOD_IC_BUILTIN_FN_ADDR_OFFSET]
        ; cmp x14, x15
        ; b.ne =>miss
        ; ldr w11, [x17, COLLECTION_METHOD_IC_LEAF_STUB_ID_OFFSET]
    );
    emit_leaf_no_alloc_stub2_pair_call(ops, 11, dst, recv, key, miss)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(true)
}

pub(super) fn emit_collection_alloc_method_guarded_call(
    ops: &mut Assembler,
    operands: impl WordOperands,
    alloc: &JitCollectionAllocMethod,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    if view.cage_base == 0 || alloc.value_arg_count != 3 {
        return Ok(false);
    }
    let Some(stub_addr) =
        alloc_value_stub_by_id(alloc.alloc_stub_id).and_then(|stub| stub.entry_addr())
    else {
        return Ok(false);
    };

    let dst = reg(operands, 0)?;
    let recv = reg(operands, 1)?;
    let argc = const_index(operands, 3)? as usize;
    let arg0 = if argc == 0 {
        None
    } else {
        Some(reg(operands, 4)?)
    };
    let arg1 = if argc <= 1 || alloc.alloc_stub_id == STUB_COLLECTION_SET_ADD_ALLOC.id {
        None
    } else {
        Some(reg(operands, 5)?)
    };
    let guard_flags_byte = view.collection_layout.guard_flags_byte;
    let object_shape_byte = view.object_shape_byte;
    let object_values_ptr_byte = view.object_values_ptr_byte;
    let native_static_fn_byte = view.native_static_fn_byte;
    let method_value_byte = alloc.method_value_byte;
    let receiver_type_tag = u32::from(alloc.receiver_type_tag);
    let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);
    let undefined_bits = VALUE_UNDEFINED;

    load_reg(ops, 9, recv)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9
    );
    emit_load_u64(ops, 13, view.cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, receiver_type_tag
        ; b.ne =>miss
        ; ldr w14, [x13, guard_flags_byte]
        ; cbnz w14, =>miss
    );

    emit_load_u64(ops, 15, view.cage_base as u64);
    emit_load_u64(ops, 12, u64::from(alloc.proto_offset));
    dynasm!(ops
        ; .arch aarch64
        ; add x15, x15, x12
        ; ldrb w14, [x15]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w14, [x15, object_shape_byte]
    );
    emit_load_u64(ops, 12, u64::from(alloc.proto_shape));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w14, w12
        ; b.ne =>miss
        ; ldr x15, [x15, object_values_ptr_byte]
        ; cbz x15, =>miss
        // The value slab holds 4-byte compressed slots, so the method value is
        // a 32-bit load (the byte offset is `slot * 4` and need not be
        // 8-aligned). The method is expected to be a cell (a native function
        // object): its low-3 tag is `000` and its zero-extended offset is the
        // bare cage offset. Any non-cell (smi / immediate / function id / boxed
        // number) or the empty slot misses to the runtime method bridge.
        ; ldr w9, [x15, method_value_byte]
        ; ands w11, w9, #0x7
        ; b.ne =>miss
        ; cbz w9, =>miss
        ; mov w12, w9
    );
    emit_load_u64(ops, 13, view.cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, native_function_type_tag
        ; b.ne =>miss
        ; ldr x14, [x13, native_static_fn_byte]
    );
    emit_load_u64(ops, 15, alloc.builtin_fn_addr as u64);
    dynasm!(ops
        ; .arch aarch64
        ; cmp x14, x15
        ; b.ne =>miss

        ; sub sp, sp, ALLOC_CTX_STACK_SIZE
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; str x10, [sp, ALLOC_CTX_FRAME_OFFSET]
        ; ldr x9, [x10, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET]
        ; str x9, [sp, ALLOC_CTX_CODE_OBJECT_ID_OFFSET]
        ; movz w9, alloc.safepoint_id
        ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
        ; str wzr, [sp, ALLOC_CTX_RESERVED0_OFFSET]
        ; movz w9, #0
        ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        ; strh w9, [sp, ALLOC_CTX_RESERVED1_OFFSET]
        ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]

        ; mov x0, sp
    );
    emit_load_u64(ops, 1, u64::from(alloc.safepoint_id));
    load_reg(ops, 2, recv)?;
    if let Some(arg0) = arg0 {
        load_reg(ops, 3, arg0)?;
    } else {
        emit_load_u64(ops, 3, undefined_bits);
    }
    if let Some(arg1) = arg1 {
        load_reg(ops, 4, arg1)?;
    } else {
        emit_load_u64(ops, 4, undefined_bits);
    }
    emit_load_u64(ops, 16, stub_addr as u64);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
        ; mov x5, x1
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
        ; cbnz x5, =>miss
    );
    store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(true)
}

pub(super) fn emit_live_collection_alloc_method_guarded_call(
    ops: &mut Assembler,
    operands: impl WordOperands,
    site: u64,
    safepoint: SafepointId,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
    done: DynamicLabel,
) -> Result<bool, Unsupported> {
    if view.cage_base == 0 {
        return Ok(false);
    }

    let dst = reg(operands, 0)?;
    let recv = reg(operands, 1)?;
    let argc = const_index(operands, 3)? as usize;
    let arg0 = if argc == 0 {
        None
    } else {
        Some(reg(operands, 4)?)
    };
    let arg1 = if argc <= 1 {
        None
    } else {
        Some(reg(operands, 5)?)
    };
    let guard_flags_byte = view.collection_layout.guard_flags_byte;
    let object_shape_byte = view.object_shape_byte;
    let object_values_ptr_byte = view.object_values_ptr_byte;
    let native_static_fn_byte = view.native_static_fn_byte;
    let native_function_type_tag = u32::from(view.collection_layout.native_function_type_tag);
    let undefined_bits = VALUE_UNDEFINED;

    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x20, COLLECTION_METHOD_ICS_OFFSET]
        ; cbz x17, =>miss
        ; ldr w10, [x20, COLLECTION_METHOD_IC_COUNT_OFFSET]
    );
    emit_load_u64(ops, 11, site);
    dynasm!(ops ; .arch aarch64 ; cmp x11, x10 ; b.hs =>miss);
    emit_load_u64(
        ops,
        12,
        site.saturating_mul(u64::from(COLLECTION_METHOD_IC_SLOT_SIZE)),
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x17, x17, x12
        ; ldrb w10, [x17, COLLECTION_METHOD_IC_STATE_OFFSET]
        ; cmp w10, JIT_COLLECTION_METHOD_IC_COLLECTION as u32
        ; b.ne =>miss
        ; ldr w11, [x17, COLLECTION_METHOD_IC_ALLOC_STUB_ID_OFFSET]
    );
    emit_load_u64(ops, 12, u64::from(JIT_COLLECTION_METHOD_IC_NO_STUB));
    dynasm!(ops ; .arch aarch64 ; cmp x11, x12 ; b.eq =>miss);

    load_reg(ops, 9, recv)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9
    );
    emit_load_u64(ops, 13, view.cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; ldrb w15, [x17, COLLECTION_METHOD_IC_RECEIVER_TYPE_TAG_OFFSET]
        ; cmp w14, w15
        ; b.ne =>miss
        ; ldr w14, [x13, guard_flags_byte]
        ; cbnz w14, =>miss
    );

    emit_load_u64(ops, 15, view.cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; ldr w12, [x17, COLLECTION_METHOD_IC_PROTO_OFFSET]
        ; add x15, x15, x12
        ; ldrb w14, [x15]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
        ; ldr w14, [x15, object_shape_byte]
        ; ldr w12, [x17, COLLECTION_METHOD_IC_PROTO_SHAPE_OFFSET]
        ; cmp w14, w12
        ; b.ne =>miss
        ; ldr x15, [x15, object_values_ptr_byte]
        ; cbz x15, =>miss
        ; ldr w12, [x17, COLLECTION_METHOD_IC_METHOD_VALUE_BYTE_OFFSET]
        ; ldr x9, [x15, x12]
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
        ; tst x9, x11
        ; b.ne =>miss
        ; mov w12, w9
    );
    emit_load_u64(ops, 13, view.cage_base as u64);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, native_function_type_tag
        ; b.ne =>miss
        ; ldr x14, [x13, native_static_fn_byte]
        ; ldr x15, [x17, COLLECTION_METHOD_IC_BUILTIN_FN_ADDR_OFFSET]
        ; cmp x14, x15
        ; b.ne =>miss
        ; ldr w1, [x17, COLLECTION_METHOD_IC_ALLOC_STUB_ID_OFFSET]

        ; sub sp, sp, ALLOC_CTX_STACK_SIZE
        ; ldr x9, [x20, THREAD_OFFSET]
        ; str x9, [sp, ALLOC_CTX_THREAD_OFFSET]
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; str x10, [sp, ALLOC_CTX_FRAME_OFFSET]
        ; ldr x9, [x10, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET]
        ; str x9, [sp, ALLOC_CTX_CODE_OBJECT_ID_OFFSET]
        ; movz w9, safepoint
        ; str w9, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET]
        ; str wzr, [sp, ALLOC_CTX_RESERVED0_OFFSET]
        ; movz w9, #0
        ; strh wzr, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET]
        ; strh w9, [sp, ALLOC_CTX_RESERVED1_OFFSET]
        ; str xzr, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]

        ; mov x0, sp
    );
    emit_load_u64(ops, 2, u64::from(safepoint));
    load_reg(ops, 3, recv)?;
    if let Some(arg0) = arg0 {
        load_reg(ops, 4, arg0)?;
    } else {
        emit_load_u64(ops, 4, undefined_bits);
    }
    if let Some(arg1) = arg1 {
        emit_load_u64(ops, 5, undefined_bits);
        let set_add = ops.new_dynamic_label();
        emit_load_u64(ops, 9, u64::from(STUB_COLLECTION_SET_ADD_ALLOC.id));
        dynasm!(ops ; .arch aarch64 ; cmp x1, x9 ; b.eq =>set_add);
        load_reg(ops, 5, arg1)?;
        dynasm!(ops ; .arch aarch64 ; =>set_add);
    } else {
        emit_load_u64(ops, 5, undefined_bits);
    }
    emit_load_u64(
        ops,
        16,
        alloc_value_stub_trampoline_pair as *const () as u64,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
        ; mov x5, x1
        ; add sp, sp, ALLOC_CTX_STACK_SIZE
        ; cbnz x5, =>miss
    );
    store_reg(ops, 0, dst)?;
    dynasm!(ops ; .arch aarch64 ; b =>done);
    Ok(true)
}

/// Emit a direct `CallMethodValue`: resolve the method through the call
/// site's monomorphic IC and direct-branch to its compiled entry, exactly
/// like [`emit_call`]; on an ineligible resolution fall back to the in-place
/// full method-call stub (not a bail) so cold / native / polymorphic methods
/// keep running compiled.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_method_call(
    ops: &mut Assembler,
    operands: impl WordOperands,
    site: u64,
    leaf: Option<&JitCollectionLeafMethod>,
    alloc: Option<&JitCollectionAllocMethod>,
    view: Option<&JitCompileSnapshot>,
    live_alloc_safepoint: Option<SafepointId>,
    bail: DynamicLabel,
    threw: DynamicLabel,
) -> Result<(), Unsupported> {
    let dst = reg(operands, 0)?;
    let recv = reg(operands, 1)?;
    let name = const_index(operands, 2)?;
    let argc = const_index(operands, 3)? as usize;
    if argc > MAX_METHOD_ARGS {
        return Err(Unsupported::ArgCount(argc));
    }
    // The argument register indices, packed one per 16-bit lane, are handed
    // to every method-call stub in a single register.
    let mut method_arg_regs: Vec<u16> = Vec::with_capacity(argc);
    for slot in 0..argc {
        method_arg_regs.push(reg(operands, 4 + slot)?);
    }
    let packed_args = pack_method_arg_regs(&method_arg_regs);

    let fallback = ops.new_dynamic_label();
    let after_leaf = ops.new_dynamic_label();
    let after_alloc = ops.new_dynamic_label();
    let after_live_leaf = ops.new_dynamic_label();
    let after_live_alloc = ops.new_dynamic_label();
    let after_direct_inline = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    if let Some(view) = view
        && emit_direct_method_inline(ops, operands, site, view, after_direct_inline, done, threw)?
    {
        dynasm!(ops ; .arch aarch64 ; =>after_direct_inline);
    }

    if let (Some(leaf), Some(view)) = (leaf, view)
        && emit_collection_leaf_method_guarded_call(ops, operands, leaf, view, after_leaf, done)?
    {
        dynasm!(ops ; .arch aarch64 ; =>after_leaf);
    }
    if let (Some(alloc), Some(view)) = (alloc, view)
        && emit_collection_alloc_method_guarded_call(ops, operands, alloc, view, after_alloc, done)?
    {
        dynasm!(ops ; .arch aarch64 ; =>after_alloc);
    }
    if let Some(view) = view
        && emit_live_collection_leaf_method_guarded_call(
            ops,
            operands,
            site,
            view,
            after_live_leaf,
            done,
        )?
    {
        dynasm!(ops ; .arch aarch64 ; =>after_live_leaf);
    }
    if let (Some(view), Some(safepoint)) = (view, live_alloc_safepoint)
        && emit_live_collection_alloc_method_guarded_call(
            ops,
            operands,
            site,
            safepoint,
            view,
            after_live_alloc,
            done,
        )?
    {
        dynasm!(ops ; .arch aarch64 ; =>after_live_alloc);
    }

    dynasm!(
        ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, dst as u32
        ; movz x2, recv as u32
    );
    emit_load_u64(ops, 3, site);
    dynasm!(ops ; .arch aarch64 ; movz x4, argc as u32);
    emit_load_u64(ops, 5, packed_args);
    emit_load_u64(
        ops,
        16,
        jit_call_collection_method_ic_stub as *const () as u64,
    );
    dynasm!(
        ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cbz x0, =>done
    );

    if leaf.is_some() || alloc.is_some() {
        dynasm!(ops ; .arch aarch64 ; b =>fallback);
    }

    // jit_prepare_direct_method_call_stub(ctx, recv, name, site, argc, a0..a2)
    // -> 0 = direct prepared, 1 = throw, 2 = ineligible → in-place fallback.
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x20
        ; movz x1, recv as u32
    );
    emit_load_u64(ops, 2, u64::from(name));
    emit_load_u64(ops, 3, site);
    dynasm!(ops ; .arch aarch64 ; movz x4, argc as u32);
    emit_load_u64(ops, 5, packed_args);
    emit_load_u64(
        ops,
        16,
        jit_prepare_direct_method_call_stub as *const () as u64,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x0, #1
        ; b.eq =>threw
        ; cmp x0, #2
        ; b.eq =>fallback
    );

    // Direct prepared (status 0): same dispatch tail as Op::Call.
    emit_direct_call_tail(ops, dst, threw, done);

    // Ineligible resolution bails to normal dispatch. Native code never
    // re-enters one interpreter opcode through a bespoke method bridge.
    dynasm!(ops ; .arch aarch64 ; =>fallback ; b =>bail ; =>done);
    Ok(())
}
