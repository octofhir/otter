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
use otter_vm::JitCompileSnapshot;

use crate::baseline::{
    CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, FUNCTION_ID_TAG, NUMBER_TAG_HI16, Unsupported,
    VALUE_FALSE, VALUE_FALSE_LOW, VALUE_HOLE, VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED, reg_offset,
};

/// Immediate forms of the tagged constants for `dynasm` compare operands.
const VALUE_UNDEFINED_IMM: u32 = VALUE_UNDEFINED as u32;
const VALUE_NULL_IMM: u32 = VALUE_NULL as u32;
const VALUE_TRUE_IMM: u32 = VALUE_TRUE as u32;
const VALUE_FALSE_IMM: u32 = VALUE_FALSE as u32;
const VALUE_HOLE_IMM: u32 = VALUE_HOLE as u32;

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

/// Compute the value-slab base for a shape-matched receiver into `x13`, which
/// holds the decompressed `GcHeader` pointer on entry (`x14` is clobbered). A
/// small object (`slab_len <= INLINE_SLOT_CAP`) carries its slab inline in the
/// body, so the base is `header + object_inline_values_byte`, derived fresh
/// from the receiver's header every access. This deliberately never reads the
/// cached `values_ptr` for inline slabs: that pointer aims into the body and
/// dangles the instant the moving collector relocates the object. A spilled
/// object's slab is a stable out-of-line allocation, so its base loads from
/// `values_ptr`.
pub(super) fn emit_slab_base(ops: &mut Assembler, view: &JitCompileSnapshot, reg: u8, scratch: u8) {
    // Frozen ABI (a `dynasm` immediate must be a compile-time constant): the
    // inline slab capacity and the header-relative offset of the in-body
    // inline slab, checked against the values otter-vm baked from the live
    // `#[repr(C)]` layout so a field reorder trips in tests.
    const INLINE_SLOT_CAP: u32 = 2;
    const INLINE_VALUES_BYTE: u32 = 80;
    debug_assert_eq!(INLINE_SLOT_CAP, view.object_inline_slot_cap);
    debug_assert_eq!(INLINE_VALUES_BYTE, view.object_inline_values_byte);
    assert_eq!((reg, scratch), (13, 14), "fixed-register slab-base form");
    let slab_len_off = view.object_slab_len_byte;
    let values_ptr_off = view.object_values_ptr_byte;
    let spilled = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldrh w14, [x13, slab_len_off]
        ; cmp w14, INLINE_SLOT_CAP
        ; b.hi =>spilled
        ; add x13, x13, INLINE_VALUES_BYTE
        ; b =>done
        ; =>spilled
        ; ldr x13, [x13, values_ptr_off]
        ; =>done
    );
}

/// Decompress a 4-byte object property slot (already zero-extended into
/// `x9`) into a full tagged `Value`, in place in `x9`.
///
/// A small-int, cell-ref, immediate, or function-id slot decodes inline; a
/// boxed slot (a heap-boxed double / wide int) branches to `boxed_bail`,
/// where the interpreter reads the box. Fixed registers: `x9` is the slot
/// in/out, `x10` scratch.
pub(super) fn emit_decompress_slot(ops: &mut Assembler, cage_base: u64, boxed_bail: DynamicLabel) {
    use otter_vm::value::compressed as cslot;
    // The literal slot tags below are the frozen `compressed` layout.
    debug_assert_eq!(cslot::TAG_MASK, 0b111);
    debug_assert_eq!(cslot::TAG_IMMEDIATE, 0b100);
    debug_assert_eq!(cslot::TAG_FUNCTION_ID, 0b110);
    debug_assert_eq!(
        (
            cslot::IMM_NULL,
            cslot::IMM_TRUE,
            cslot::IMM_FALSE,
            cslot::IMM_HOLE
        ),
        (1, 2, 3, 4)
    );
    let l_smi = ops.new_dynamic_label();
    let l_cell = ops.new_dynamic_label();
    let l_imm = ops.new_dynamic_label();
    let l_fid = ops.new_dynamic_label();
    let l_undef = ops.new_dynamic_label();
    let l_null = ops.new_dynamic_label();
    let l_true = ops.new_dynamic_label();
    let l_false = ops.new_dynamic_label();
    let l_hole = ops.new_dynamic_label();
    let l_done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; tbnz w9, #0, =>l_smi                      // bit0 set → small int
        ; and w10, w9, #0x7                         // low-3-bit slot tag
        ; cbz w10, =>l_cell                         // 000 → cell ref
        ; cmp w10, #0x4                             // 100 → immediate
        ; b.eq =>l_imm
        ; cmp w10, #0x6                             // 110 → function id
        ; b.eq =>l_fid
        ; b =>boxed_bail                            // 010 → boxed number
        ; =>l_cell
        // A cell ref widens to the canonical heap-cell `Value` bits:
        // `cage_base | offset`. The empty slot (0) decodes to `undefined`.
        ; cbz x9, =>l_undef
    );
    emit_load_u64(ops, 10, cage_base);
    dynasm!(ops
        ; .arch aarch64
        ; orr x9, x9, x10
        ; b =>l_done
        ; =>l_smi
        ; asr w9, w9, #1                            // int32 = (i31 << 1 | 1) >> 1
        ; mov w9, w9                                // zero-extend the payload
        ; movz x10, NUMBER_TAG_HI16, lsl #48
        ; orr x9, x9, x10
        ; b =>l_done
        ; =>l_fid
        ; lsr w9, w9, #3                            // function id
        ; lsl x9, x9, #16
        ; movz x10, FUNCTION_ID_TAG as u32          // fits a single movz
        ; orr x9, x9, x10
        ; b =>l_done
        ; =>l_imm
        ; lsr w10, w9, #3                           // immediate kind
        ; cmp w10, #1                               // IMM_NULL
        ; b.eq =>l_null
        ; cmp w10, #2                               // IMM_TRUE
        ; b.eq =>l_true
        ; cmp w10, #3                               // IMM_FALSE
        ; b.eq =>l_false
        ; cmp w10, #4                               // IMM_HOLE
        ; b.eq =>l_hole
        ; =>l_undef
        ; movz x9, VALUE_UNDEFINED as u32
        ; b =>l_done
        ; =>l_null
        ; movz x9, VALUE_NULL as u32
        ; b =>l_done
        ; =>l_true
        ; movz x9, VALUE_TRUE as u32
        ; b =>l_done
        ; =>l_false
        ; movz x9, VALUE_FALSE as u32
        ; b =>l_done
        ; =>l_hole
        ; movz x9, VALUE_HOLE as u32
        ; =>l_done
    );
}

/// Compress the tagged `Value` in `x9` into a 4-byte object slot in `w10`.
/// Handles the barrier-free, non-allocating cases: a small int in
/// `[-2^30, 2^30)` and the `undefined` / `null` / boolean / hole immediates.
/// A wide int, double, function id, or heap cell branches to `bail` (the
/// window transition re-runs the store — a boxed number allocates, a cell
/// needs the write barrier). The caller has already excluded cells. Fixed
/// registers: `x9` value in, `w10` compressed slot out, `x11` scratch.
pub(super) fn emit_compress_slot_or_bail(ops: &mut Assembler, bail: DynamicLabel) {
    use otter_vm::value::compressed as cslot;
    // The literal compressed-immediate words below are `(kind << 3) | 0b100`.
    debug_assert_eq!(cslot::TAG_IMMEDIATE, 0b100);
    debug_assert_eq!(cslot::IMM_UNDEFINED, 0);
    let not_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let imm_undef = ops.new_dynamic_label();
    let imm_null = ops.new_dynamic_label();
    let imm_true = ops.new_dynamic_label();
    let imm_false = ops.new_dynamic_label();
    let imm_hole = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; and x10, x9, x11
        ; cmp x10, x11
        ; b.ne =>not_int                            // not an int32
        // int32: keep only a small int in [-2^30, 2^30); wider ints box.
        ; movz w11, #0x4000, lsl #16                // 2^30
        ; add w10, w9, w11
        ; tbnz w10, #31, =>bail                     // out of small-int range
        ; lsl w10, w9, #1
        ; orr w10, w10, #1                          // (i << 1) | 1
        ; b =>done
        ; =>not_int
        ; cmp x9, VALUE_UNDEFINED_IMM
        ; b.eq =>imm_undef
        ; cmp x9, VALUE_NULL_IMM
        ; b.eq =>imm_null
        ; cmp x9, VALUE_TRUE_IMM
        ; b.eq =>imm_true
        ; cmp x9, VALUE_FALSE_IMM
        ; b.eq =>imm_false
        ; cmp x9, VALUE_HOLE_IMM
        ; b.eq =>imm_hole
        ; b =>bail                                  // double / function id
        ; =>imm_undef
        ; movz w10, #0x4                            // (0 << 3) | 0b100
        ; b =>done
        ; =>imm_null
        ; movz w10, #0xc                            // (1 << 3) | 0b100
        ; b =>done
        ; =>imm_true
        ; movz w10, #0x14                           // (2 << 3) | 0b100
        ; b =>done
        ; =>imm_false
        ; movz w10, #0x1c                           // (3 << 3) | 0b100
        ; b =>done
        ; =>imm_hole
        ; movz w10, #0x24                           // (4 << 3) | 0b100
        ; =>done
    );
}
