//! AArch64 machine-code backend for the baseline pipeline.
//!
//! # Contents
//! - Dynasm templates for supported bytecode operations.
//! - Function, bailout, runtime-stub, and OSR emission.
//! - AArch64-specific guards, boxing, and inline-cache fast paths.
//!
//! # Invariants
//! - Input has passed the backend-neutral BaselinePlan prepass.
//! - Allocating calls use planned safepoints and published frame roots.
//! - Embedded data pointers are retained by EmissionArtifacts.
//!
//! # See also
//! - super::lowering for validation and planning.
//! - super::code for finalized code ownership and VM entry.

#![allow(unused_parens)]
use super::{
    ALLOC_CTX_CODE_OBJECT_ID_OFFSET, ALLOC_CTX_FRAME_OFFSET, ALLOC_CTX_RESERVED0_OFFSET,
    ALLOC_CTX_RESERVED1_OFFSET, ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET,
    ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET,
    ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET, BACKEDGE_FUEL_OFFSET, BaselineCode, BaselinePlan,
    CANONICAL_NAN_HI16, COLLECTION_METHOD_IC_ALLOC_STUB_ID_OFFSET,
    COLLECTION_METHOD_IC_BUILTIN_FN_ADDR_OFFSET, COLLECTION_METHOD_IC_COUNT_OFFSET,
    COLLECTION_METHOD_IC_LEAF_STUB_ID_OFFSET, COLLECTION_METHOD_IC_METHOD_VALUE_BYTE_OFFSET,
    COLLECTION_METHOD_IC_PROTO_OFFSET, COLLECTION_METHOD_IC_PROTO_SHAPE_OFFSET,
    COLLECTION_METHOD_IC_RECEIVER_TYPE_TAG_OFFSET, COLLECTION_METHOD_IC_SLOT_SIZE,
    COLLECTION_METHOD_IC_STATE_OFFSET, COLLECTION_METHOD_ICS_OFFSET, DIRECT_ENTRY_OFFSET,
    DIRECT_FRAME_INDEX_OFFSET, DIRECT_METHOD_ENTRY_OFFSET, DIRECT_METHOD_FID_OFFSET,
    DIRECT_METHOD_INLINE_OFFSET, DIRECT_METHOD_INLINE_SLOT_SIZE, DIRECT_METHOD_ON_RECEIVER_OFFSET,
    DIRECT_METHOD_PROTO_SHAPE_OFFSET, DIRECT_METHOD_RECV_SHAPE_OFFSET,
    DIRECT_METHOD_REGISTER_COUNT_OFFSET, DIRECT_METHOD_VALUE_BYTE_OFFSET, DIRECT_REGS_OFFSET,
    DIRECT_SELF_OFFSET, DIRECT_THIS_OFFSET, DIRECT_UPVALUES_OFFSET, DOUBLE_OFFSET_HI16,
    ERROR_SLOT_OFFSET, EmissionArtifacts, FRAME_INDEX_OFFSET, FUNCTION_ID_TAG, GC_HEAP_OFFSET,
    IC_WAYS, INTERRUPT_FLAG_OFFSET, JIT_CTX_STACK_SIZE, JS_CLOSURE_BODY_TYPE_TAG, MAX_INLINE_ARGS,
    MAX_METHOD_ARGS, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET, NATIVE_FRAME_OFFSET, NUMBER_TAG_HI16,
    OBJECT_BODY_TYPE_TAG, Op, REG_STACK_BASE_OFFSET, REG_TOP_PTR_OFFSET, RESUME_PC_OFFSET,
    STATUS_BAILED, STATUS_RETURNED, STATUS_THREW, SYNC_REENTRY_DEPTH_PTR_OFFSET,
    SYNC_REENTRY_LIMIT_OFFSET, THIS_VALUE_OFFSET, THREAD_OFFSET, UPVALUE_CELL_SIZE,
    UPVALUE_VALUE_OFFSET, UPVALUES_PTR_OFFSET, Unsupported, VALUE_FALSE, VALUE_FALSE_LOW,
    VALUE_HOLE, VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED, WordOperands,
    alloc_value_stub_trampoline_pair, branch_target, const_index, imm32,
    jit_abort_direct_call_stub, jit_add_stub, jit_backedge_poll_stub,
    jit_call_collection_method_ic_stub, jit_define_data_property_stub,
    jit_define_own_property_stub, jit_direct_method_call_bail_stub,
    jit_finish_direct_call_bailed_stub, jit_finish_direct_call_returned_stub,
    jit_fresh_upvalue_stub, jit_inline_closure_upvalues_stub, jit_load_builtin_error_stub,
    jit_load_element_stub, jit_load_global_stub, jit_load_prop_window_stub, jit_load_string_stub,
    jit_load_upvalue_stub, jit_make_closure_stub, jit_make_fn_stub, jit_math_call_stub,
    jit_neg_stub, jit_new_array_stub, jit_new_object_stub, jit_pop_native_activation_stub,
    jit_prepare_direct_method_call_stub, jit_push_native_activation_stub, jit_self_call_bail_stub,
    jit_store_element_stub, jit_store_prop_window_stub, jit_store_upvalue_checked_stub,
    jit_store_upvalue_stub, jit_write_barrier_stub, jit_write_barrier_window_stub,
    leaf_no_alloc_stub2_trampoline_pair, local_index, otter_jit_math_random, pack_method_arg_regs,
    reg, reg_offset, reg3, value_tag,
};
use crate::CompiledCode;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::Interpreter;
use otter_vm::{
    JitArrayMethod, JitArrayMethodKind, JitCollectionAllocMethod, JitCollectionLeafMethod,
    JitCompileSnapshot, JitInlineCallee, JitInlineMethod, JitTypedArrayLayout,
    STUB_COLLECTION_SET_ADD_ALLOC, STUB_STRING_CONCAT_ALLOC, SafepointId,
    jit::{JIT_COLLECTION_METHOD_IC_COLLECTION, JIT_COLLECTION_METHOD_IC_NO_STUB},
    runtime_stubs::alloc_value_stub_by_id,
};
use std::collections::BTreeMap;

#[macro_use]
mod arithmetic;
mod assembler;
mod calls;
mod elements;
mod emitter;
use arithmetic::*;
use assembler::*;
use calls::*;
pub(super) use elements::vec_layout_offsets;
use elements::{emit_array_store, emit_element_load, emit_ta_guard_chain};
pub(super) use emitter::compile;

/// Emit the function prologue: save fp/lr + callee-saved bases, then set
/// `x20 = ctx` (arg in `x0`) and `x19 = ctx.regs` (the frame register base).
/// Shared by the main entry and every OSR trampoline so both honor the same
/// [`JitEntry`] ABI.
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

/// Emit `blr` to a Rust stub at `addr` and branch to `threw` on nonzero
/// status. The stub's argument registers (`x0`..) must already be set.
fn emit_call_stub(ops: &mut Assembler, addr: usize, threw: DynamicLabel) {
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

fn emit_backedge_interrupt_check(ops: &mut Assembler, threw: DynamicLabel) {
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
