//! AArch64 tagged-value encode/decode primitives for the template backend.
//!
//! # Contents
//! - Register-window loads/stores and 64-bit immediate materialization.
//! - Number guards, int32/double boxing, and NaN-purifying double encode.
//! - Full-semantics `ToInt32`/`ToUint32` fast paths for bitwise operators.
//!
//! # Invariants
//! - Every helper documents its scratch registers; nothing survives a call.
//! - Boxed doubles are purified before encoding, so no emitted value aliases
//!   the cell space.
//! - Coercions the fast path cannot represent exactly branch to the caller's
//!   side-exit label before any observable effect.
//!
//! # See also
//! - `otter_vm::value::tag` — the frozen boxed-value contract these bake.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};

use crate::baseline::{
    CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, NUMBER_TAG_HI16, Unsupported, VALUE_FALSE_LOW,
    reg_offset,
};

/// `ldr X(t), [x19, #idx*8]`.
pub(super) fn emit_load_reg(ops: &mut Assembler, t: u8, idx: u16) -> Result<(), Unsupported> {
    let off = reg_offset(idx)?;
    dynasm!(ops ; .arch aarch64 ; ldr X(t), [x19, off]);
    Ok(())
}

/// `str X(t), [x19, #idx*8]`.
pub(super) fn emit_store_reg(ops: &mut Assembler, t: u8, idx: u16) -> Result<(), Unsupported> {
    let off = reg_offset(idx)?;
    dynasm!(ops ; .arch aarch64 ; str X(t), [x19, off]);
    Ok(())
}

/// Materialize a 64-bit constant into x-register `t` via movz/movk.
pub(super) fn emit_load_u64(ops: &mut Assembler, t: u8, v: u64) {
    dynasm!(ops ; .arch aarch64 ; movz X(t), (v & 0xFFFF) as u32);
    if (v >> 16) & 0xFFFF != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(t), ((v >> 16) & 0xFFFF) as u32, lsl #16);
    }
    if (v >> 32) & 0xFFFF != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(t), ((v >> 32) & 0xFFFF) as u32, lsl #32);
    }
    if (v >> 48) & 0xFFFF != 0 {
        dynasm!(ops ; .arch aarch64 ; movk X(t), ((v >> 48) & 0xFFFF) as u32, lsl #48);
    }
}

/// Box the int32 payload in the low 32 bits of `X(t)` by setting the number
/// tag. The producing op wrote `X(t)` through its `W` view, which zeroes bits
/// [63:32], so a single `orr` completes the box. Clobbers `X(scratch)`.
pub(super) fn emit_box_int32(ops: &mut Assembler, t: u8, scratch: u8) {
    dynasm!(ops
        ; .arch aarch64
        ; movz X(scratch), NUMBER_TAG_HI16, lsl #48
        ; orr X(t), X(t), X(scratch)
    );
}

/// Box a boolean: a preceding `cset` wrote `0`/`1` into `W(t)`; adding
/// `VALUE_FALSE` yields the full `false`/`true` immediate word. Clobbers
/// `W(scratch)`.
pub(super) fn emit_box_bool(ops: &mut Assembler, t: u8, scratch: u8) {
    dynasm!(ops
        ; .arch aarch64
        ; movz W(scratch), VALUE_FALSE_LOW
        ; add W(t), W(t), W(scratch)
    );
}

/// Guard that `X(r)` is an int32 immediate: branch to `bail` unless every
/// number-tag bit is set. Clobbers x14/x15.
pub(super) fn emit_guard_int32(ops: &mut Assembler, r: u8, bail: DynamicLabel) {
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, X(r), x15
        ; cmp x14, x15
        ; b.ne =>bail
    );
}

/// Guard that `X(r)` is a Number (int32 or boxed double): branch to `bail`
/// when no number-tag bit is set. Clobbers x15.
pub(super) fn emit_guard_number(ops: &mut Assembler, r: u8, bail: DynamicLabel) {
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; tst X(r), x15
        ; b.eq =>bail
    );
}

/// Decode the `Number` in x-register `src_x` into f64 register `dst_d`.
///
/// `int32` payloads sign-convert (`scvtf`); a boxed double has the encode
/// offset subtracted before `fmov`; a cell or non-number immediate (no
/// number-tag bit) branches to `bail`. Uses scratch GPRs x14/x15.
pub(super) fn emit_num_to_double(ops: &mut Assembler, src_x: u8, dst_d: u8, bail: DynamicLabel) {
    let is_non_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, X(src_x), x15
        ; cmp x14, x15
        ; b.ne =>is_non_int
        ; scvtf D(dst_d), W(src_x)          // int32: signed 32-bit → f64
        ; b =>done
        ; =>is_non_int
        // A boxed double carries at least one number-tag bit; a cell or
        // tagged immediate carries none and bails for exact coercion.
        ; tst X(src_x), x15
        ; b.eq =>bail
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x14, X(src_x), x14
        ; fmov D(dst_d), x14
        ; =>done
    );
}

/// Box the f64 in register `src_d` into x-register `dst_x` as a `Value`.
///
/// A NaN result is first canonicalised to the single quiet-NaN pattern;
/// then the encode offset is added so the bits land in the number space.
/// Uses scratch GPR x14 in addition to `dst_x`.
pub(super) fn emit_box_double(ops: &mut Assembler, src_d: u8, dst_x: u8) {
    let ready = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; fmov X(dst_x), D(src_d)
        ; fcmp D(src_d), D(src_d)
        ; b.vc =>ready                       // ordered (not NaN) → keep bits
        ; movz X(dst_x), CANONICAL_NAN_HI16, lsl #48
        ; =>ready
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; add X(dst_x), X(dst_x), x14        // purify into the number space
    );
}

/// Fast-path `ToInt32` for bitwise operators.
///
/// Int32-tagged values are unboxed directly. Any finite double is truncated
/// toward zero and reduced modulo 2^32 — the full ECMAScript `ToInt32`, not
/// just the already-in-range case. Only NaN / infinity / `|x| >= 2^63`
/// (which would saturate the 64-bit `fcvtzs`) and non-number tags branch to
/// `bail` for exact coercion. Clobbers x14/x15, d0–d2.
pub(super) fn emit_to_int32_fast(ops: &mut Assembler, src_x: u8, dst_w: u8, bail: DynamicLabel) {
    emit_to_int32_common(ops, src_x, dst_w, bail);
}

/// Fast-path `ToUint32` for unsigned shifts.
///
/// Identical machine sequence to [`emit_to_int32_fast`]: the truncated i64's
/// low 32 bits are the `mod 2^32` residue either way; only the consumer's
/// signedness interpretation differs.
pub(super) fn emit_to_uint32_fast(ops: &mut Assembler, src_x: u8, dst_w: u8, bail: DynamicLabel) {
    emit_to_int32_common(ops, src_x, dst_w, bail);
}

fn emit_to_int32_common(ops: &mut Assembler, src_x: u8, dst_w: u8, bail: DynamicLabel) {
    let is_non_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; and x14, X(src_x), x15
        ; cmp x14, x15
        ; b.ne =>is_non_int
        ; mov W(dst_w), W(src_x)
        ; b =>done
        ; =>is_non_int
        // A boxed double carries at least one number-tag bit; a cell or
        // tagged immediate carries none and bails for exact coercion. The
        // canonical NaN flows to the fcmp check below and bails as non-finite.
        ; tst X(src_x), x15
        ; b.eq =>bail
        ; movz x14, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x14, X(src_x), x14      // unbox double
        ; fmov d0, x14
        ; fcmp d0, d0
        ; b.vs =>bail
    );
    // A finite double with `|x| < 2^63` truncates toward zero into i64
    // exactly (`fcvtzs`, round-to-zero); its low 32 bits are the value mod
    // 2^32. Only `|x| >= 2^63` / infinity would saturate `fcvtzs`, so those
    // bail.
    emit_load_u64(ops, 14, 9_223_372_036_854_775_808.0f64.to_bits());
    dynasm!(ops
        ; .arch aarch64
        ; fabs d1, d0
        ; fmov d2, x14
        ; fcmp d1, d2
        ; b.ge =>bail
        ; fcvtzs X(dst_w), d0
        ; =>done
    );
}
