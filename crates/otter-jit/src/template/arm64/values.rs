//! AArch64 tagged-value encode/decode primitives for the template backend.
//!
//! # Contents
//! - Register-window loads/stores, 64-bit immediate materialization, and typed
//!   symbolic-address capture.
//! - Number guards, int32/double boxing, and NaN-purifying double encode.
//! - Full-semantics `ToInt32`/`ToUint32` fast paths for bitwise operators.
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

/// Materialize a 64-bit constant into x-register `t` via movz/movk.
pub(crate) fn emit_load_u64(ops: &mut Assembler, t: u8, v: u64) {
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

/// Materialize one process-local symbolic value without changing the
/// variable-width `movz`/`movk` sequence used by the normal code path.
pub(crate) fn emit_load_symbol_u64(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    t: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    emit_load_u64(ops, t, value);
    relocations.record_mov_wide(start, ops.offset().0, t, target);
}

/// Materialize a validated runtime-stub entry and attach its descriptor.
pub(super) fn emit_load_runtime_stub(
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
/// tag nor the immediate tag, so the whole test is one mask and one `tst`. The
/// scratch register is explicit because callers hold live values in different
/// places; the mask is materialized rather than written as a logical immediate
/// because `dynasm` accepts one only against a literal register.
pub(crate) fn emit_cell_test(
    ops: &mut Assembler,
    value: u8,
    scratch: u8,
    test: CellTest,
    target: DynamicLabel,
) {
    emit_load_u64(ops, scratch, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch aarch64 ; tst X(value), X(scratch));
    match test {
        CellTest::IsCell => dynasm!(ops ; .arch aarch64 ; b.eq =>target),
        CellTest::IsNotCell => dynasm!(ops ; .arch aarch64 ; b.ne =>target),
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
    debug_assert_ne!(src_d, 0, "d0 is this helper's scratch register");
    let double_path = ops.new_dynamic_label();
    let box_int = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; fcvtzs w14, D(src_d)              // trunc toward zero, saturating
        ; scvtf d0, w14                     // back to f64
        ; fcmp D(src_d), d0
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
/// small object (null out-of-line slab handle) carries its slab inline in the
/// body, so the base is `header + object_inline_values_byte`, derived fresh
/// from the receiver's header every access. This deliberately never reads the
/// cached `values_ptr` for inline slabs: that pointer aims into the body and
/// dangles the instant the moving collector relocates the object. A spilled
/// object's slab is a stable out-of-line allocation, so its base loads from
/// `values_ptr`.
pub(crate) fn emit_slab_base(ops: &mut Assembler, view: &JitCompileSnapshot, reg: u8, scratch: u8) {
    // Frozen ABI (a `dynasm` immediate must be a compile-time constant): the
    // inline slab capacity and the header-relative offset of the in-body
    // inline slab, checked against the values otter-vm baked from the live
    // `#[repr(C)]` layout so a field reorder trips in tests.
    const INLINE_VALUES_BYTE: u32 = 64;
    const SLAB_HANDLE_BYTE: u32 = 24;
    debug_assert_eq!(INLINE_VALUES_BYTE, view.object_inline_values_byte);
    debug_assert_eq!(SLAB_HANDLE_BYTE, view.object_slab_handle_byte);
    assert_eq!((reg, scratch), (13, 14), "fixed-register slab-base form");
    let values_ptr_off = view.object_values_ptr_byte;
    let spilled = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // Branch on the out-of-line slab HANDLE, not on `slab_len`: the
    // capacity model can move a `len <= INLINE_SLOT_CAP` object's slots
    // out of line (an existing-slot slow store reserves ahead), and a
    // spilled slab that shrinks back stays out of line — a length
    // compare reads the stale in-body copy in both cases.
    dynasm!(ops
        ; .arch aarch64
        ; ldr w14, [x13, SLAB_HANDLE_BYTE]
        ; cbnz w14, =>spilled
        ; add x13, x13, INLINE_VALUES_BYTE
        ; b =>done
        ; =>spilled
        ; ldr x13, [x13, values_ptr_off]
        ; =>done
    );
}

/// Initialize a receiver's cached value-slab base once generated code has
/// appended its first own slot.
///
/// The VM keeps `values_ptr` current for every mutation. An inline object
/// (null out-of-line slab handle) points it at the in-body array as soon as
/// slot zero exists, while a spilled object already points at its stable
/// out-of-line slab — receiver preparation reserves such a slab ahead of a
/// multi-field transition program, before any `StoreProperty` runs. Only the
/// inline case may be written here: rewriting a spilled object's base would
/// aim every later slab-relative store into the body past its three inline
/// words. `header` holds the receiver's `GcHeader` pointer in `x13`; `x16` is
/// clobbered.
pub(crate) fn emit_initialize_inline_values_ptr(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    header: u8,
    scratch: u8,
) {
    assert_eq!(
        (header, scratch),
        (13, 16),
        "fixed-register values-pointer initialization form"
    );
    let ready = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldr w16, [x13, view.object_slab_handle_byte]
        ; cbnz w16, =>ready
        ; add x16, x13, view.object_inline_values_byte
        ; str x16, [x13, view.object_values_ptr_byte]
        ; =>ready
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
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    value: u8,
    scratch_a: u8,
    scratch_b: u8,
    exit: DynamicLabel,
) {
    if view.cage_base == 0 {
        emit_cell_test(ops, value, scratch_a, CellTest::IsCell, exit);
        return;
    }
    let fall_through = ops.new_dynamic_label();
    emit_cell_test(ops, value, scratch_a, CellTest::IsNotCell, fall_through);
    emit_load_symbol_u64(
        ops,
        relocations,
        scratch_a,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; mov W(scratch_b), W(value)
        ; add X(scratch_a), X(scratch_a), X(scratch_b)
        ; ldrb W(scratch_b), [X(scratch_a)]
    );
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
/// Clobbers `x0`, `x1`, `x2`, `x14`, `x15` and `x16`.
pub(crate) fn emit_write_barrier(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    parent: u8,
    child: u8,
) {
    emit_write_barrier_with_context(ops, relocations, view, parent, child, 20);
}

/// Context-register-parametric form used by the Machine IR backend, whose
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
        ; mov w15, W(child)
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        14,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x14, x14, x15
        ; ldrb w14, [x14, flags_byte]
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
