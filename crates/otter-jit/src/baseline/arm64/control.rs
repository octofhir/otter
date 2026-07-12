//! AArch64 function-entry, runtime-call, and polling control primitives.
//!
//! # Contents
//! - Baseline entry prologue and common epilogue.
//! - Typed bridge invocation and throw branching.
//! - Shape-slot slab address selection.
//! - Cooperative backedge interrupt/fuel polling.
//!
//! # Invariants
//! - Prologue and every OSR trampoline establish the identical JitEntry ABI.
//! - Runtime calls branch immediately when the shared error slot is populated.
//! - Decompressed slab pointers are used only in non-safepoint regions.

use super::*;

/// Emit the function prologue: save fp/lr + callee-saved bases, then set
/// `x20 = ctx` (arg in `x0`) and `x19 = ctx.regs` (the frame register base).
/// Shared by the main entry and every OSR trampoline so both honor the same
/// [`JitEntry`] ABI.
pub(super) fn emit_prologue(ops: &mut Assembler) {
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
pub(super) fn emit_epilogue(ops: &mut Assembler) {
    dynasm!(ops
        ; .arch aarch64
        ; ldp x19, x20, [sp, #16]
        ; ldp x29, x30, [sp], #32
        ; ret
    );
}
/// Emit `blr` to a Rust stub at `addr` and branch to `threw` on nonzero
/// status. The stub's argument registers (`x0`..) must already be set.
pub(super) fn emit_call_stub(ops: &mut Assembler, addr: usize, threw: DynamicLabel) {
    emit_load_u64(ops, 16, addr as u64);
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbnz x0, =>threw
    );
}

/// Compute the value-slab base for a shape-matched receiver into `reg`, which
/// holds the decompressed `GcHeader` pointer on entry (`scratch` is
/// clobbered). A small object (`slab_len <= INLINE_SLOT_CAP`) carries its slab
/// inline in the body, so the base is `header + object_inline_values_byte`,
/// derived fresh from the receiver's header every access. This deliberately
/// never reads the cached `values_ptr`: that pointer aims into the body and
/// dangles the instant the moving collector relocates the object — a stale
/// base the collector only re-caches lazily, so a compiled load/store that
/// trusted it wrote through a freed slab. A spilled object's slab is a stable
/// out-of-line allocation, so its base is loaded from `values_ptr`.
pub(crate) fn emit_slab_base(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    reg: u32,
    scratch: u32,
) {
    // Frozen ABI (a `dynasm` immediate must be a compile-time constant): the
    // inline slab capacity and the header-relative offset of the in-body
    // inline slab. Pinned to `INLINE_SLOT_CAP` and
    // `HEADER_SIZE + OBJECT_BODY_INLINE_VALUES_OFFSET`, `debug_assert`ed
    // against the values otter-vm baked from the live `#[repr(C)]` layout so a
    // field reorder trips in tests rather than baking a wild offset.
    const INLINE_SLOT_CAP: u32 = 2;
    const INLINE_VALUES_BYTE: u32 = 80;
    debug_assert_eq!(INLINE_SLOT_CAP, view.object_inline_slot_cap);
    debug_assert_eq!(INLINE_VALUES_BYTE, view.object_inline_values_byte);
    let slab_len_off = view.object_slab_len_byte;
    let values_ptr_off = view.object_values_ptr_byte;
    let spilled = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // A `dynasm` `cmp` / `add` immediate is only accepted with a static
    // register operand, so emit the fixed-register form for each register
    // pair the two emitters call this with (baseline x13/x14, optimizing
    // x16/x17).
    match (reg, scratch) {
        (13, 14) => dynasm!(ops
            ; .arch aarch64
            ; ldrh w14, [x13, slab_len_off]
            ; cmp w14, INLINE_SLOT_CAP
            ; b.hi =>spilled
            ; add x13, x13, INLINE_VALUES_BYTE
            ; b =>done
            ; =>spilled
            ; ldr x13, [x13, values_ptr_off]
            ; =>done
        ),
        (16, 17) => dynasm!(ops
            ; .arch aarch64
            ; ldrh w17, [x16, slab_len_off]
            ; cmp w17, INLINE_SLOT_CAP
            ; b.hi =>spilled
            ; add x16, x16, INLINE_VALUES_BYTE
            ; b =>done
            ; =>spilled
            ; ldr x16, [x16, values_ptr_off]
            ; =>done
        ),
        _ => unreachable!("emit_slab_base register pair"),
    }
}

pub(super) fn emit_backedge_interrupt_check(ops: &mut Assembler, threw: DynamicLabel) {
    let slow = ops.new_dynamic_label();
    let cont = ops.new_dynamic_label();
    // Inline cooperative poll: read the interrupt byte and decrement the fuel
    // counter, re-entering the poll stub only when the interrupt is set or the
    // counter reaches zero. x9/x10 are transient scratch (no value is live
    // across a block boundary in the baseline register-window model).
    dynasm!(ops
        ; .arch aarch64
        ; ldr x9, [x20, INTERRUPT_FLAG_OFFSET]
        ; ldrb w9, [x9]
        ; cbnz w9, =>slow
        ; ldr x9, [x20, BACKEDGE_FUEL_OFFSET]
        ; ldr x10, [x9]
        ; subs x10, x10, #1
        ; str x10, [x9]
        ; b.gt =>cont
        ; =>slow
        ; mov x0, x20
    );
    emit_call_stub(ops, jit_backedge_poll_stub as *const () as usize, threw);
    dynasm!(ops ; .arch aarch64 ; =>cont);
}
