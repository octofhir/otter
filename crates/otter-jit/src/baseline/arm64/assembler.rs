//! Low-level AArch64 assembler primitives shared by opcode emitters.
//!
//! # Contents
//! - Register-window loads and stores.
//! - Immediate materialization and number boxing/unboxing.
//! - Compressed object-slot encoding and decoding.
//!
//! # Invariants
//! - Register accesses are validated by backend-neutral lowering rules.
//! - Heap slot helpers never retain decompressed pointers across safepoints.
//! - Scratch-register contracts are documented at each primitive.

use super::{
    CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, FUNCTION_ID_TAG, NUMBER_TAG_HI16, Unsupported,
    VALUE_FALSE, VALUE_HOLE, VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED, reg_offset,
};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};

/// `ldr X(t), [x19, #idx*8]`.
pub(super) fn load_reg(ops: &mut Assembler, t: u8, idx: u16) -> Result<(), Unsupported> {
    let off = reg_offset(idx)?;
    dynasm!(ops ; .arch aarch64 ; ldr X(t), [x19, off]);
    Ok(())
}
/// `str X(t), [x19, #idx*8]`.
pub(super) fn store_reg(ops: &mut Assembler, t: u8, idx: u16) -> Result<(), Unsupported> {
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

/// Decode the `Number` in x-register `src_x` into f64 register `dst_d`.
///
/// `int32` payloads sign-convert (`scvtf`); a boxed double has the encode
/// offset subtracted before `fmov`; a cell or non-number immediate (no
/// `NUMBER_TAG` bit) bails to the interpreter. Uses scratch GPRs x14/x15.
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
        // A boxed double carries at least one NUMBER_TAG bit; a cell or
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

/// Decompress a 4-byte object property slot (already zero-extended into
/// `x9`) into a full tagged `Value`, in place in `x9`.
///
/// A small-int, cell-ref, immediate, or function-id slot decodes inline; a
/// `TAG_BOXED` slot (a heap-boxed double / wide int) branches to
/// `boxed_bail`, where the interpreter reads the box. Fixed registers (the
/// `#imm` forms below require literal registers): `x9` is the slot in/out,
/// `x10` is scratch.
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
        // `cage_base | offset` (`Value::from_cell_offset`). A bare offset
        // would still dereference (consumers rebuild the address from the
        // low 32 bits) but would never bit-compare equal to a canonically
        // boxed handle of the same object, breaking strict/loose equality
        // on any value that flowed through a compiled slot load. The empty
        // slot (0) decodes to `undefined`.
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
        ; movz x10, FUNCTION_ID_TAG as u32          // 0x22, fits a single movz
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

/// Compress the tagged `Value` in `X(value)` into a 4-byte object slot in
/// `W(out)`. Handles the barrier-free, non-allocating cases: a small int in
/// `[-2^30, 2^30)` and the `undefined` / `null` / boolean / hole immediates.
/// A wide int, double, function id, or heap cell branches to `bail` (the
/// interpreter re-runs the store — a boxed number allocates, a cell needs the
/// write barrier). The caller has already excluded cells. Clobbers `X(sc)`.
/// Fixed registers (the `#imm` forms require literal registers): `x9` is the
/// value in, `w10` the compressed slot out, `x11` scratch.
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
        ; cmp x9, #(VALUE_UNDEFINED as u32)
        ; b.eq =>imm_undef
        ; cmp x9, #(VALUE_NULL as u32)
        ; b.eq =>imm_null
        ; cmp x9, #(VALUE_TRUE as u32)
        ; b.eq =>imm_true
        ; cmp x9, #(VALUE_FALSE as u32)
        ; b.eq =>imm_false
        ; cmp x9, #(VALUE_HOLE as u32)
        ; b.eq =>imm_hole
        ; b =>bail                                  // double / function id → interpreter
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
