//! AArch64 dynasm backend for the template compiler.
//!
//! # Contents
//! - Code-buffer ownership, per-PC labels, and branch fixups.
//! - Prologue/epilogue establishing the shared compiled-entry ABI.
//! - One emit dispatch per [`TemplateOp`], including inline tagged truthiness.
//! - Cooperative back-edge interrupt/fuel polling.
//! - [`values`] — tagged encode/decode primitives.
//! - [`arith`] — numeric, comparison, and bitwise emitters.
//!
//! # Invariants
//! - Every instruction stamps its canonical resume PC into the published
//!   native frame before any observable work; exact side exits, returns, and
//!   the throw status are the only exits.
//! - Tagged truthiness decides numbers, booleans, `null`, and `undefined`
//!   inline; heap cells and every other encoding take an exact side exit so
//!   the interpreter re-executes the uncommitted instruction.
//! - Boxed-double falsiness is decided by exact bit patterns (`+0.0`, `-0.0`,
//!   the canonical NaN); the VM's NaN-purification invariant makes this
//!   complete.
//! - Emitted code bakes only offsets from the shared entry-ABI module and the
//!   frozen value-tag contract; no Rust container layout is probed.
//!
//! # See also
//! - [`super::plan`] — the validated operation stream consumed here.
//! - [`super::code`] — the owner of the finalized mapping.

// dynasm 5 normalizes dynamic AArch64 register operands through `Into<u8>`;
// when our register ids are already `u8`, that macro-generated conversion is
// intentionally redundant and outside the source-level emitter's control.
#![allow(clippy::useless_conversion)]

mod arith;
mod values;

use std::collections::BTreeMap;

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;

use self::arith::{
    emit_binary_arith, emit_compare, emit_increment, emit_int_bitwise, emit_loose_compare,
    emit_negate, emit_to_numeric, emit_to_primitive, emit_unsigned_shift_right,
};
use self::values::{emit_load_reg, emit_load_u64, emit_store_reg};
use super::{TemplateCode, TemplateOp, TemplatePlan};
use crate::CompiledCode;
use crate::baseline::{
    CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET,
    NUMBER_TAG_HI16, STATUS_BAILED, STATUS_RETURNED, STATUS_THREW, THREAD_OFFSET, Unsupported,
    VALUE_FALSE, VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET,
    VM_THREAD_INTERRUPT_CELL_OFFSET, jit_backedge_poll_stub, reg_offset,
};

/// Boolean/nullish immediates as 32-bit `dynasm` operands.
const VALUE_TRUE_IMM: u32 = VALUE_TRUE as u32;
const VALUE_FALSE_IMM: u32 = VALUE_FALSE as u32;
const VALUE_NULL_IMM: u32 = VALUE_NULL as u32;
const VALUE_UNDEFINED_IMM: u32 = VALUE_UNDEFINED as u32;

pub(super) fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
) -> Result<TemplateCode, Unsupported> {
    let plan = TemplatePlan::build(view)?;
    let mut ops = Assembler::new().expect("assembler alloc");
    let bail = ops.new_dynamic_label();
    let threw = ops.new_dynamic_label();
    let labels: BTreeMap<u32, DynamicLabel> = plan
        .instructions
        .iter()
        .map(|instr| (instr.pc, ops.new_dynamic_label()))
        .collect();

    let entry = ops.offset();
    emit_prologue(&mut ops);
    for instr in &plan.instructions {
        let label = labels[&instr.pc];
        dynasm!(ops ; .arch aarch64 ; =>label);
        // Publish this op's logical PC before any observable work; the
        // published NativeFrame PC is the one canonical logical PC every bail
        // path and diagnostic reads.
        emit_stamp_pc(&mut ops, instr.pc);
        match instr.op {
            TemplateOp::LoadImmediate { dst, bits } => {
                emit_load_u64(&mut ops, 9, bits);
                emit_store_reg(&mut ops, 9, dst)?;
            }
            TemplateOp::Move { dst, src } => {
                emit_load_reg(&mut ops, 9, src)?;
                emit_store_reg(&mut ops, 9, dst)?;
            }
            TemplateOp::Jump { target, back_edge } => {
                let tgt = labels[&target];
                if back_edge {
                    emit_backedge_poll(&mut ops, threw);
                }
                dynasm!(ops ; .arch aarch64 ; b =>tgt);
            }
            TemplateOp::Branch {
                condition,
                target,
                when_truthy,
                back_edge,
            } => {
                let tgt = labels[&target];
                emit_load_reg(&mut ops, 9, condition)?;
                emit_truthiness_bool(&mut ops, bail);
                dynasm!(ops ; .arch aarch64 ; cmp x9, VALUE_TRUE_IMM);
                if back_edge {
                    let taken = ops.new_dynamic_label();
                    let fallthrough = ops.new_dynamic_label();
                    if when_truthy {
                        dynasm!(ops ; .arch aarch64 ; b.eq =>taken);
                    } else {
                        dynasm!(ops ; .arch aarch64 ; b.ne =>taken);
                    }
                    dynasm!(ops ; .arch aarch64 ; b =>fallthrough ; =>taken);
                    emit_backedge_poll(&mut ops, threw);
                    dynasm!(ops ; .arch aarch64 ; b =>tgt ; =>fallthrough);
                } else if when_truthy {
                    dynasm!(ops ; .arch aarch64 ; b.eq =>tgt);
                } else {
                    dynasm!(ops ; .arch aarch64 ; b.ne =>tgt);
                }
            }
            TemplateOp::Truthiness { dst, src, negate } => {
                emit_load_reg(&mut ops, 9, src)?;
                emit_truthiness_bool(&mut ops, bail);
                if negate {
                    // VALUE_TRUE and VALUE_FALSE differ exactly in bit 0.
                    dynasm!(ops ; .arch aarch64 ; eor x9, x9, #1);
                }
                emit_store_reg(&mut ops, 9, dst)?;
            }
            TemplateOp::BinaryArith {
                dst,
                lhs,
                rhs,
                kind,
            } => {
                emit_binary_arith(&mut ops, dst, lhs, rhs, kind, bail)?;
            }
            TemplateOp::Compare {
                dst,
                lhs,
                rhs,
                kind,
            } => {
                emit_compare(&mut ops, dst, lhs, rhs, kind, bail)?;
            }
            TemplateOp::LooseCompare {
                dst,
                lhs,
                rhs,
                negate,
            } => {
                emit_loose_compare(&mut ops, dst, lhs, rhs, negate, bail)?;
            }
            TemplateOp::IntBitwise {
                dst,
                lhs,
                rhs,
                kind,
            } => {
                emit_int_bitwise(&mut ops, dst, lhs, rhs, kind, bail)?;
            }
            TemplateOp::UnsignedShiftRight { dst, lhs, rhs } => {
                emit_unsigned_shift_right(&mut ops, dst, lhs, rhs, bail)?;
            }
            TemplateOp::Increment { dst, src, delta } => {
                emit_increment(&mut ops, dst, src, delta, bail)?;
            }
            TemplateOp::Negate { dst, src } => {
                emit_negate(&mut ops, dst, src, bail)?;
            }
            TemplateOp::ToNumeric { dst, src } => {
                emit_to_numeric(&mut ops, dst, src, bail)?;
            }
            TemplateOp::ToPrimitive { dst, src } => {
                emit_to_primitive(&mut ops, dst, src, bail)?;
            }
            TemplateOp::Return { src } => {
                let off = reg_offset(src)?;
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr x0, [x19, off]
                    ; movz x1, STATUS_RETURNED as u32
                );
                emit_epilogue(&mut ops);
            }
            TemplateOp::ReturnUndefined => {
                emit_load_u64(&mut ops, 0, VALUE_UNDEFINED);
                dynasm!(ops ; .arch aarch64 ; movz x1, STATUS_RETURNED as u32);
                emit_epilogue(&mut ops);
            }
        }
    }

    // Shared exact-side-exit epilogue: status = bailed, value = 0. The frame
    // PC stamped at the exiting instruction names the uncommitted opcode.
    dynasm!(ops
        ; .arch aarch64
        ; =>bail
        ; movz x0, #0
        ; movz x1, STATUS_BAILED as u32
    );
    emit_epilogue(&mut ops);
    // Shared throw epilogue: the poll stub parked the error in the context.
    dynasm!(ops
        ; .arch aarch64
        ; =>threw
        ; movz x0, #0
        ; movz x1, STATUS_THREW as u32
    );
    emit_epilogue(&mut ops);

    let buf = ops.finalize().expect("finalize");
    Ok(TemplateCode::from_emission(
        CompiledCode::new(buf, entry),
        code_object_id,
        view.code_block.id,
        plan.register_count,
    ))
}

/// Emit the function prologue: save fp/lr + callee-saved bases, then set
/// `x20 = ctx` (arg in `x0`) and `x19 = ctx.regs` (the frame register base) —
/// the shared compiled-entry ABI.
fn emit_prologue(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch aarch64
        ; stp x29, x30, [sp, #-32]!
        ; stp x19, x20, [sp, #16]
        ; mov x29, sp
        ; mov x20, x0
        ; ldr x19, [x20]
    );
}

/// Emit the function epilogue (restore callee-saved + frame, return). `x0`
/// (value) and `x1` (status) must already be set.
fn emit_epilogue(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch aarch64
        ; ldp x19, x20, [sp, #16]
        ; ldp x29, x30, [sp], #32
        ; ret
    );
}

/// Publish the canonical instruction-index PC into the active native frame.
fn emit_stamp_pc(ops: &mut Assembler, pc: u32) {
    emit_load_u64(ops, 9, u64::from(pc));
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
        ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
    );
}

/// Reduce the tagged `Value` in `x9` to `VALUE_TRUE` / `VALUE_FALSE` in `x9`
/// per `ToBoolean`. Numbers (int32 and boxed double), booleans, `null`, and
/// `undefined` decide inline; a heap cell or any other encoding (the hole,
/// function-id immediates) branches to `bail` for the exact side exit.
/// Clobbers `x14`/`x15`.
fn emit_truthiness_bool(ops: &mut Assembler, bail: DynamicLabel) {
    let int_case = ops.new_dynamic_label();
    let double_case = ops.new_dynamic_label();
    let truthy = ops.new_dynamic_label();
    let falsy = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, x9, x15
        ; cmp x14, x15
        ; b.eq =>int_case                       // all tag bits → int32
        ; cbnz x14, =>double_case               // some tag bits → boxed double
        ; cmp x9, VALUE_TRUE_IMM
        ; b.eq =>truthy
        ; cmp x9, VALUE_FALSE_IMM
        ; b.eq =>falsy
        ; cmp x9, VALUE_NULL_IMM
        ; b.eq =>falsy
        ; cmp x9, VALUE_UNDEFINED_IMM
        ; b.eq =>falsy
        ; b =>bail                              // cell / hole / other encoding
        ; =>int_case
        ; cbz w9, =>falsy
        ; b =>truthy
        ; =>double_case
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x14, x9, x14                      // raw f64 bit pattern
        ; cbz x14, =>falsy                      // +0.0
        ; movz x15, #0x8000, lsl #48
        ; cmp x14, x15
        ; b.eq =>falsy                          // -0.0
        ; movz x15, CANONICAL_NAN_HI16, lsl #48
        ; cmp x14, x15
        ; b.eq =>falsy                          // canonical NaN
        ; =>truthy
        ; movz x9, VALUE_TRUE_IMM
        ; b =>done
        ; =>falsy
        ; movz x9, VALUE_FALSE_IMM
        ; =>done
    );
}

/// Inline cooperative poll at a back edge: read the interrupt byte and
/// decrement the fuel counter, re-entering the poll stub only when the
/// interrupt is set or the counter reaches zero. A nonzero stub status
/// branches to the throw epilogue.
fn emit_backedge_poll(ops: &mut Assembler, threw: DynamicLabel) {
    let slow = ops.new_dynamic_label();
    let cont = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr x17, [x20, THREAD_OFFSET]
        ; ldr x9, [x17, VM_THREAD_INTERRUPT_CELL_OFFSET]
        ; ldrb w9, [x9]
        ; cbnz w9, =>slow
        ; ldr x9, [x17, VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET]
        ; ldr x10, [x9]
        ; subs x10, x10, #1
        ; str x10, [x9]
        ; b.gt =>cont
        ; =>slow
        ; mov x0, x20
    );
    emit_load_u64(ops, 16, jit_backedge_poll_stub as *const () as u64);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
        ; =>cont
    );
}
