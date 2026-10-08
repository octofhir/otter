//! AArch64 tagged-value encode/decode primitives for the template backend.
//!
//! # Contents
//! - Register-window loads/stores, 64-bit immediate materialization, and typed
//!   symbolic-address capture.
//! - Number guards, int32/double boxing, and NaN-purifying double encode.
//! - Full-semantics `ToInt32`/`ToUint32` fast paths for bitwise operators.
//! - Object shape reads: the prototype and immutable state the shape fixes.
//!
//! # Invariants
//! - Every helper documents its scratch registers; nothing survives a call.
//! - Boxed doubles are purified before encoding, so no emitted value aliases
//!   the cell space.
//! - Coercions the fast path cannot represent exactly branch to the caller's
//!   supplied pre-effect continuation, normally an outlined VM transition.
//! - Symbolic loads wrap the ordinary variable-width materializer, so capture
//!   never changes installed machine code.
//!
//! # See also
//! - `otter_vm::value::tag` — the frozen boxed-value contract these bake.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;

use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, NUMBER_TAG_HI16, THREAD_OFFSET, Unsupported,
    VALUE_FALSE_LOW, VM_THREAD_GC_HEAP_OFFSET, VM_THREAD_MARKING_FLAG_CELL_OFFSET, reg_offset,
};

/// `ldr X(t), [x19, #idx*8]`.
pub(crate) fn emit_load_reg(ops: &mut Assembler, t: u8, idx: u16) -> Result<(), Unsupported> {
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

/// Materialize a 64-bit constant into x-register `t` in the fewest
/// instructions, as V8's `MacroAssembler::Mov` does: one `movz` or `movn`
/// when a single halfword differs from the rest, one `orr` of a logical
/// immediate, otherwise `movz` or `movn` of the first halfword plus a `movk`
/// for each other halfword that differs from that fill.
pub(crate) fn emit_load_u64(ops: &mut Assembler, t: u8, v: u64) {
    let halfword = |index: u32| ((v >> (16 * index)) & 0xFFFF) as u32;
    let zeros = (0..4).filter(|&index| halfword(index) == 0).count();
    let ones = (0..4).filter(|&index| halfword(index) == 0xFFFF).count();
    if zeros < 3 && ones < 3 && dynasmrt::aarch64::encode_logical_immediate_64bit(v).is_some() {
        dynasm!(ops ; .arch aarch64 ; orr XSP(t), xzr, v);
        return;
    }
    // Fill with ones when more halfwords are all ones than all zeros.
    let inverted = ones > zeros;
    let fill = if inverted { 0xFFFF } else { 0 };
    let mut first = true;
    for index in 0..4 {
        let part = halfword(index);
        if part == fill && !(index == 3 && first) {
            continue;
        }
        if first {
            let value = if inverted { !part & 0xFFFF } else { part };
            match (inverted, index) {
                (false, 0) => dynasm!(ops ; .arch aarch64 ; movz X(t), value),
                (false, 1) => dynasm!(ops ; .arch aarch64 ; movz X(t), value, lsl #16),
                (false, 2) => dynasm!(ops ; .arch aarch64 ; movz X(t), value, lsl #32),
                (false, _) => dynasm!(ops ; .arch aarch64 ; movz X(t), value, lsl #48),
                (true, 0) => dynasm!(ops ; .arch aarch64 ; movn X(t), value),
                (true, 1) => dynasm!(ops ; .arch aarch64 ; movn X(t), value, lsl #16),
                (true, 2) => dynasm!(ops ; .arch aarch64 ; movn X(t), value, lsl #32),
                (true, _) => dynasm!(ops ; .arch aarch64 ; movn X(t), value, lsl #48),
            }
            first = false;
            continue;
        }
        match index {
            1 => dynasm!(ops ; .arch aarch64 ; movk X(t), part, lsl #16),
            2 => dynasm!(ops ; .arch aarch64 ; movk X(t), part, lsl #32),
            _ => dynasm!(ops ; .arch aarch64 ; movk X(t), part, lsl #48),
        }
    }
}

/// The fixed `movz` + `movk` form of [`emit_load_u64`] whose instructions a
/// relocation record names and a later rewrite patches.
pub(crate) fn emit_load_u64_wide(ops: &mut Assembler, t: u8, v: u64) {
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

/// Materialize one process-local symbolic value in the fixed `movz`/`movk`
/// form its relocation record names.
pub(crate) fn emit_load_symbol_u64(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    t: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    emit_load_u64_wide(ops, t, value);
    relocations.record_mov_wide(start, ops.offset().0, t, target);
}

/// Check one retained chain validity word before any dependent effect.
pub(crate) fn emit_prototype_validity_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    validity: otter_vm::jit::JitPrototypeValidity,
    scratch: u8,
    miss: DynamicLabel,
) {
    emit_load_symbol_u64(
        ops,
        relocations,
        scratch,
        validity.address as u64,
        RelocationTarget::PrototypeValidityCell {
            identity: validity.identity,
        },
    );
    dynasm!(ops ; .arch aarch64 ; ldar W(scratch), [X(scratch)] ; cbz W(scratch), =>miss);
}

/// Materialize a validated runtime-stub entry and attach its descriptor.
pub(crate) fn emit_load_runtime_stub(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    t: u8,
    value: u64,
    descriptor: otter_vm::native_abi::RuntimeStubDescriptor,
) {
    emit_load_symbol_u64(
        ops,
        relocations,
        t,
        value,
        RelocationTarget::runtime_stub(descriptor),
    );
}

/// Which way a cell test branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellTest {
    /// Branch when the value is a heap cell.
    IsCell,
    /// Branch when the value is anything else: a number or a tagged immediate.
    IsNotCell,
}

/// Branch to `target` when `X(value)`'s cell-ness matches `test`.
///
/// A boxed `Value` is a heap cell exactly when it carries neither the number
/// tag nor the immediate tag. The mask is a high run plus bit 1, so the test
/// is one logical-immediate `tst` and one bit test, with no scratch register.
pub(crate) fn emit_cell_test(ops: &mut Assembler, value: u8, test: CellTest, target: DynamicLabel) {
    const _: () = assert!(otter_vm::value::tag::NOT_CELL_MASK == 0xfffe_0000_0000_0002);
    match test {
        CellTest::IsCell => {
            let not_cell = ops.new_dynamic_label();
            dynasm!(ops ; .arch aarch64
                ; tst X(value), #0xfffe_0000_0000_0000 ; b.ne =>not_cell
                ; tbz X(value), #1, =>target
                ; =>not_cell);
        }
        CellTest::IsNotCell => dynasm!(ops ; .arch aarch64
            ; tst X(value), #0xfffe_0000_0000_0000 ; b.ne =>target
            ; tbnz X(value), #1, =>target),
    }
}

/// Box the int32 payload in the low 32 bits of `X(t)` by setting the number
/// tag. The producing op wrote `X(t)` through its `W` view, which zeroes bits
/// [63:32], so a single `orr` completes the box. Clobbers `X(scratch)`.
pub(crate) fn emit_box_int32(ops: &mut Assembler, t: u8, scratch: u8) {
    dynasm!(ops
        ; .arch aarch64
        ; movz X(scratch), NUMBER_TAG_HI16, lsl #48
        ; orr X(t), X(t), X(scratch)
    );
}

/// Box a boolean: a preceding `cset` wrote `0`/`1` into `W(t)`; adding
/// `VALUE_FALSE` yields the full `false`/`true` immediate word. Clobbers
/// `W(scratch)`.
pub(crate) fn emit_box_bool(ops: &mut Assembler, t: u8, scratch: u8) {
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

/// Decode the `Number` in x-register `src_x` into f64 register `dst_d`.
///
/// `int32` payloads sign-convert (`scvtf`); a boxed double has the encode
/// offset subtracted before `fmov`; a cell or non-number immediate (no
/// number-tag bit) branches to `bail`. Uses scratch GPRs x14/x15.
pub(crate) fn emit_num_to_double(ops: &mut Assembler, src_x: u8, dst_d: u8, bail: DynamicLabel) {
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
pub(crate) fn emit_box_double(ops: &mut Assembler, src_d: u8, dst_x: u8) {
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

/// Box the f64 in `src_d` into `dst_x` with the exact tag the per-op
/// arithmetic path would produce for that value.
///
/// A value that is integral, inside the signed 32-bit range, and not `-0`
/// boxes as an `int32` (matching the no-overflow integer fast path so
/// downstream `int32`-guarded sites stay hot); every other number — a
/// fractional value, one outside `i32` range, `-0`, `NaN`, or `±Inf` — boxes
/// as a double, `NaN`-canonicalised exactly like [`emit_box_double`]. This is
/// the representation linchpin for fusing a chain of double operations: the
/// fused result carries the same tag the unfused per-op sequence left behind.
///
/// `fcvtzs` saturates out-of-range and non-finite inputs, so the
/// round-trip compare (`fcvtzs` then `scvtf` then `fcmp`) is `true` only for a
/// value already representable as its truncated `i32` — folding the integral
/// and in-range checks into one compare. `-0` truncates to `0` and would pass
/// that compare, so it is excluded explicitly by its sign bit.
///
/// Reads `src_d`; writes `dst_x`. Clobbers `d0`, `x14`, `x15`. `src_d` must
/// not be `d0`.
pub(crate) fn emit_box_number(ops: &mut Assembler, src_d: u8, dst_x: u8) {
    emit_box_number_with_scratch(ops, src_d, dst_x, 0);
}

/// Box a canonical Number with a caller-selected non-allocatable FP scratch.
///
/// This is the same representation contract as [`emit_box_number`]. The
/// separate scratch parameter lets an optimizing node keep every declared
/// floating-point clobber outside its allocatable register file.
pub(crate) fn emit_box_number_with_scratch(
    ops: &mut Assembler,
    src_d: u8,
    dst_x: u8,
    scratch_d: u8,
) {
    debug_assert_ne!(src_d, scratch_d, "source cannot alias FP scratch");
    let double_path = ops.new_dynamic_label();
    let box_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; fcvtzs w14, D(src_d)              // trunc toward zero, saturating
        ; scvtf D(scratch_d), w14           // back to f64
        ; fcmp D(src_d), D(scratch_d)
        ; b.ne =>double_path                // non-integral / out of range / NaN
        // Integral and in range. Exclude -0: it truncates to 0 but must box as
        // a double so `1 / -0 === -Infinity` is preserved.
        ; cbnz w14, =>box_int               // non-zero payload cannot be -0
        ; fmov x15, D(src_d)
        ; tbnz x15, #63, =>double_path       // sign bit set with zero magnitude → -0
        ; =>box_int
        ; mov W(dst_x), w14                  // zero-extends bits [63:32]
        ; movz x15, NUMBER_TAG_HI16, lsl #48
        ; orr X(dst_x), X(dst_x), x15
        ; b =>done
        ; =>double_path
    );
    emit_box_double(ops, src_d, dst_x);
    dynasm!(ops ; .arch aarch64 ; =>done);
}

/// Fast-path `ToInt32` for bitwise operators.
///
/// Int32-tagged values are unboxed directly. Any finite double is truncated
/// toward zero and reduced modulo 2^32 — the full ECMAScript `ToInt32`, not
/// just the already-in-range case. With `FJCVTZS` every double converts in
/// place; otherwise NaN / infinity / `|x| >= 2^63` (which would saturate the
/// 64-bit `fcvtzs`) branch to `bail`, as do non-number tags, for exact
/// coercion. Clobbers x14/x15, d0–d2.
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
    );
    // `FJCVTZS` is the whole ECMAScript ToInt32 of any double, NaN and the
    // infinities included.
    if crate::arm64::has_javascript_conversion() {
        crate::arm64::emit_fjcvtzs(ops, 0, dst_w);
        dynasm!(ops ; .arch aarch64 ; =>done);
        return;
    }
    dynasm!(ops
        ; .arch aarch64
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

/// Load into `W(dst)` the compressed `[[Prototype]]` of the object whose
/// decompressed `GcHeader` pointer is in `X(object)`: the prototype word of
/// its shape, an ordinary object or null. `X(cage)` holds the cage base;
/// `dst` may be `object`.
pub(crate) fn emit_load_prototype(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    dst: u8,
    object: u8,
    cage: u8,
) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr W(dst), [X(object), view.object_shape_byte]
        ; add X(dst), X(cage), X(dst)
        ; ldr W(dst), [X(dst), view.shape_prototype_byte]
    );
}

/// Load the sole immutable state byte of the object's current shape.
///
/// `header` is the full object header address. `state` and `cage` are distinct
/// caller-selected scratch registers; this helper preserves `header` and owns
/// the symbolic cage relocation. The result is zero-extended in `W(state)`.
/// Published object cells always carry a non-null shape; pending allocation
/// shells must not reach this helper before their shape is installed.
pub(crate) fn emit_load_shape_state(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    header: u8,
    state: u8,
    cage: u8,
) {
    assert!(header != state && header != cage && state != cage);
    emit_load_symbol_u64(
        ops,
        relocations,
        cage,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch aarch64
        ; ldr W(state), [X(header), view.object_shape_byte]
        ; add X(state), X(cage), X(state)
        ; ldrb W(state), [X(state), view.shape_state_byte]
    );
}

/// Select the shape-proven field bank without a per-object storage branch.
pub(crate) fn emit_field_base(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    reg: u8,
    scratch: u8,
    field: otter_vm::object::FieldLocation,
) {
    assert_ne!(reg, scratch);
    if field.is_inline() {
        dynasm!(ops ; .arch aarch64 ; add XSP(reg), XSP(reg), view.field_layout.inline_values_byte);
    } else {
        dynasm!(ops ; .arch aarch64 ; ldr W(scratch), [X(reg), view.field_layout.slab_handle_byte]);
        emit_load_symbol_u64(
            ops,
            relocations,
            reg,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(ops ; .arch aarch64 ; add X(reg), X(reg), X(scratch)
            ; add XSP(reg), XSP(reg), view.field_layout.slab_words_byte);
    }
}

/// Classify `X(value)` into its `typeof` kind code
/// ([`otter_bytecode::TypeOfKind`] discriminant) in `W(out)`, branching to
/// `slow` for the cells only the heap can decide: a plain object whose
/// sidecar may carry a native `[[Call]]`, a native function that may be
/// `[[IsHTMLDDA]]`, a proxy, or an internal body. Numbers, immediates and
/// every other cell resolve from the value bits and the GC type tag (V8's
/// `TestTypeOf` lowering). Clobbers `W(scratch)`.
pub(crate) fn emit_typeof_kind(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    value: u8,
    out: u8,
    scratch: u8,
    slow: DynamicLabel,
) {
    use otter_bytecode::TypeOfKind as K;
    use otter_vm::value::tag;
    let tags = otter_vm::jit::JIT_TYPEOF_TAGS;
    let done = ops.new_dynamic_label();
    let not_number = ops.new_dynamic_label();
    let immediate = ops.new_dynamic_label();
    let plain_object = ops.new_dynamic_label();
    let object = ops.new_dynamic_label();
    let function = ops.new_dynamic_label();
    let undefined = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; tst X(value), tag::NUMBER_TAG
        ; b.eq =>not_number
        ; movz W(out), K::Number as u32
        ; b =>done
        ; =>not_number
        ; tst X(value), tag::OTHER_TAG
        ; b.ne =>immediate
        ; cbz X(value), =>slow
        ; ldrb W(scratch), [X(value)]
        ; cmp WSP(scratch), u32::from(tags.plain_object)
        ; b.eq =>plain_object
        ; cmp WSP(scratch), u32::from(tags.string)
        ; b.ne >not_string
        ; movz W(out), K::String as u32
        ; b =>done
        ; not_string:
    );
    for tag_byte in tags.functions {
        dynasm!(ops ; .arch aarch64 ; cmp WSP(scratch), u32::from(tag_byte) ; b.eq =>function);
    }
    for tag_byte in tags.slow {
        dynasm!(ops ; .arch aarch64 ; cmp WSP(scratch), u32::from(tag_byte) ; b.eq =>slow);
    }
    dynasm!(ops
        ; .arch aarch64
        ; cmp WSP(scratch), u32::from(tags.symbol)
        ; b.ne >not_symbol
        ; movz W(out), K::Symbol as u32
        ; b =>done
        ; not_symbol:
        ; cmp WSP(scratch), u32::from(tags.bigint)
        ; b.ne =>object
        ; movz W(out), K::BigInt as u32
        ; b =>done
        ; =>plain_object
        ; ldr W(scratch), [X(value), view.object_exotic_handle_byte]
        ; cbnz W(scratch), =>slow
        ; =>object
        ; movz W(out), K::Object as u32
        ; b =>done
        ; =>function
        ; movz W(out), K::Function as u32
        ; b =>done
        ; =>immediate
        ; cmp XSP(value), tag::VALUE_UNDEFINED as u32
        ; b.eq =>undefined
        ; cmp XSP(value), tag::VALUE_HOLE as u32
        ; b.eq =>undefined
        ; cmp XSP(value), tag::VALUE_NULL as u32
        ; b.eq =>object
        ; and XSP(scratch), X(value), 0xffff_ffff_ffff_fffe
        ; cmp XSP(scratch), tag::VALUE_FALSE as u32
        ; b.ne >not_boolean
        ; movz W(out), K::Boolean as u32
        ; b =>done
        ; not_boolean:
        ; and XSP(scratch), X(value), 0xffff
        ; cmp XSP(scratch), tag::FUNCTION_ID_TAG as u32
        ; b.eq =>function
        ; b =>slow
        ; =>undefined
        ; movz W(out), K::Undefined as u32
        ; =>done
    );
}

/// Branch to `exit` when `X(value)` is the one cell kind whose loose equality
/// with `null`/`undefined` its tag does not decide.
///
/// Only a native-function body can carry Annex B `[[IsHTMLDDA]]`, so a nullish
/// comparison against every other cell is definitively false and needs no
/// runtime decision; a native-function cell leaves that decision to the
/// canonical comparison behind `exit`. Non-cells fall through untouched.
/// Without a cage base no cell can be inspected, so every cell exits. Clobbers
/// `X(scratch_a)` and `X(scratch_b)`.
pub(crate) fn emit_html_dda_candidate_exit(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    value: u8,
    scratch_a: u8,
    scratch_b: u8,
    exit: DynamicLabel,
) {
    let fall_through = ops.new_dynamic_label();
    emit_cell_test(ops, value, CellTest::IsNotCell, fall_through);
    // A cell value is its header's full address.
    dynasm!(ops ; .arch aarch64 ; ldrb W(scratch_b), [X(value)]);
    emit_load_u64(
        ops,
        scratch_a,
        u64::from(view.collection_layout.native_function_type_tag),
    );
    dynasm!(ops
        ; .arch aarch64
        ; cmp W(scratch_b), W(scratch_a)
        ; b.eq =>exit
        ; =>fall_through
    );
}

/// Run the write barrier a pointer store owes.
///
/// `parent` holds the guarded receiver's `GcHeader` address and `child` the
/// stored cell `Value`. The barrier has exactly two reasons to need the
/// runtime, and both are one flag test away: a marking cycle is in progress
/// (the insertion half has to shade the child), or the store really creates an
/// old->young edge whose parent is not yet in the remembered set. Everything
/// else — a young parent, a parent already recorded this scavenge interval, an
/// old child, a null child — falls straight through.
///
/// The runtime half is a leaf taking those two words directly, so even the
/// slow path publishes no frame and re-enters nothing.
///
/// The fast path clobbers `x14`–`x16`; the slow path is an AAPCS call and
/// clobbers every caller-saved register (`x0`–`x18`, `d0`–`d7`, `d16`–`d31`).
pub(crate) fn emit_write_barrier(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    parent: u8,
    child: u8,
) {
    emit_write_barrier_with_context(ops, relocations, view, parent, child, 20);
}

/// Context-register-parametric form used by the optimizing tier, whose
/// generated-function ABI keeps the [`JitCtx`](otter_vm::JitCtx) in `x19`.
pub(crate) fn emit_write_barrier_with_context(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    parent: u8,
    child: u8,
    context: u8,
) {
    let flags_byte = view.gc_barrier.header_flags_byte;
    let young = view.gc_barrier.young_flag;
    let settled = young | view.gc_barrier.remembered_flag;
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cbz W(child), =>done
        ; ldr x14, [X(context), THREAD_OFFSET]
        ; ldr x14, [x14, VM_THREAD_MARKING_FLAG_CELL_OFFSET]
        ; ldrb w14, [x14]
        ; cbnz w14, =>slow
        ; ldrb w14, [X(parent), flags_byte]
        ; movz w15, settled
        ; tst w14, w15
        ; b.ne =>done
        // An old, unrecorded parent: only a nursery child owes the remembered
        // set an entry.
    );
    // A cell value is its header's full address.
    dynasm!(ops
        ; .arch aarch64
        ; ldrb w14, [X(child), flags_byte]
        ; movz w15, young
        ; tst w14, w15
        ; b.eq =>done
        ; =>slow
        ; mov x1, X(parent)
        ; mov x2, X(child)
        ; ldr x0, [X(context), THREAD_OFFSET]
        ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
    );
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        otter_vm::runtime_stubs::WRITE_BARRIER_MUTATING.entry_addr() as u64,
        otter_vm::native_abi::STUB_WRITE_BARRIER,
    );
    dynasm!(ops ; .arch aarch64 ; blr x16 ; =>done);
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynasmrt::{AssemblyOffset, ExecutableBuffer};
    use otter_vm::value::tag;

    /// Finalize a leaf function `fn(f64) -> u64` that boxes its argument
    /// through [`emit_box_number`], exercising the real emitted machine code.
    /// The argument arrives in `d0`; it is moved to `d16` so the helper's `d0`
    /// scratch does not alias the source, mirroring how the chain emitter keeps
    /// its accumulator in a high vector register.
    fn box_number_program() -> (ExecutableBuffer, AssemblyOffset) {
        let mut ops = Assembler::new().expect("assembler");
        let entry = ops.offset();
        dynasm!(ops ; .arch aarch64 ; fmov d16, d0);
        emit_box_number(&mut ops, 16, 0);
        dynasm!(ops ; .arch aarch64 ; ret);
        let buffer = ops.finalize().expect("finalize");
        (buffer, entry)
    }

    fn run_box_number(input: f64) -> u64 {
        let (buffer, entry) = box_number_program();
        // SAFETY: the emitted leaf matches `extern "C" fn(f64) -> u64` (arg in
        // d0, result in x0) and is invoked while `buffer` is alive.
        let boxed: extern "C" fn(f64) -> u64 = unsafe { std::mem::transmute(buffer.ptr(entry)) };
        boxed(input)
    }

    #[test]
    fn box_number_matches_per_op_representation() {
        // Small integral value → int32 tag, exactly the no-overflow int path.
        assert_eq!(run_box_number(5.0), tag::box_int32(5));
        assert_eq!(run_box_number(-7.0), tag::box_int32(-7));
        assert_eq!(run_box_number(0.0), tag::box_int32(0));
        assert_eq!(run_box_number(i32::MIN as f64), tag::box_int32(i32::MIN));
        assert_eq!(run_box_number(i32::MAX as f64), tag::box_int32(i32::MAX));

        // One past the signed 32-bit range → double, not a wrapped int32.
        assert_eq!(
            run_box_number(i32::MAX as f64 + 1.0),
            tag::box_double((i32::MAX as f64 + 1.0).to_bits())
        );
        assert_eq!(
            run_box_number(i32::MIN as f64 - 1.0),
            tag::box_double((i32::MIN as f64 - 1.0).to_bits())
        );

        // -0 must stay a double so `1 / -0 === -Infinity` survives.
        assert_eq!(run_box_number(-0.0), tag::box_double((-0.0f64).to_bits()));

        // Fractional, infinities → double.
        assert_eq!(run_box_number(3.5), tag::box_double(3.5f64.to_bits()));
        assert_eq!(
            run_box_number(f64::INFINITY),
            tag::box_double(f64::INFINITY.to_bits())
        );
        assert_eq!(
            run_box_number(f64::NEG_INFINITY),
            tag::box_double(f64::NEG_INFINITY.to_bits())
        );

        // NaN → the single canonical quiet-NaN pattern, like `emit_box_double`.
        let nan = run_box_number(f64::from_bits(0x7ff8_0000_0000_0001));
        assert!(!tag::is_int32_bits(nan), "NaN must not box as int32");
        assert!(tag::is_number_bits(nan), "NaN must box as a number");
        let decoded = f64::from_bits(nan.wrapping_sub(tag::DOUBLE_ENCODE_OFFSET));
        assert!(decoded.is_nan(), "boxed NaN decodes to NaN");
    }
}
