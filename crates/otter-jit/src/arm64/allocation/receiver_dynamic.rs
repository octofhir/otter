//! Actual-family receivers in the shared generated callee prefix.
//!
//! # Contents
//! - Existing boxed LeafValue2 admission and ContextWords feedback commit.
//! - Variable-capacity initialization through the common LAB owner.
//!
//! # Invariants
//! Frame, actuals and tagged homes are published first. Only a nonzero int32
//! Probe Success reaches writes. No call/GC separates full initialization and
//! one LAB publication. Ticket stores preserve DerivedThis. Pre-effect misses
//! prepare once; any post-publication source failure is Fatal, never replay.
//!
//! # See also
//! - `otter_vm::constructor_layout` owns preparation, sampling and feedback.
//! - `super` owns LAB bump, publication and per-type accounting.

use super::{emit_bump_probe, emit_publish};
use crate::template::arm64::values::{emit_load_runtime_stub, emit_load_symbol_u64, emit_load_u64};
use crate::{
    allocation::LabRegisters,
    artifact::relocation::{RelocationCapture, RelocationTarget},
    entry::{
        NATIVE_FRAME_THIS_OFFSET, OBJECT_BODY_TYPE_TAG, RECEIVER_ALLOC_GENERATED_OFFSET,
        RUNTIME_STATS_OFFSET, THREAD_OFFSET, TransitionTable, VALUE_UNDEFINED,
        VM_THREAD_GC_HEAP_OFFSET,
    },
};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

const LAB: LabRegisters = LabRegisters {
    buffer: 11,
    candidate: 0,
    end: 13,
    scratch: 15,
    size: 17,
};
fn count(ops: &mut Assembler, byte: u32) {
    dynasm!(ops ; .arch aarch64 ; ldr x15, [x20, RUNTIME_STATS_OFFSET]
        ; ldr x14, [x15, byte] ; add x14, x14, #1 ; str x14, [x15, byte]);
}

/// x20 context/x21 frame are pinned. Caller preserves its live x6 promotion
/// decision; other volatile GP/FP values are dead before the body.
pub(crate) fn emit_dynamic_construct_receiver(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    view: &JitCompileSnapshot,
    complete: DynamicLabel,
    canonical: DynamicLabel,
) {
    let hit = ops.new_dynamic_label();
    let space = ops.new_dynamic_label();
    let fatal = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; ldr x0, [x20, THREAD_OFFSET] ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
        ; ldr x1, [x21, abi::NATIVE_FRAME_NEW_TARGET_OFFSET]
        ; ldr w2, [x21, abi::NATIVE_FRAME_FUNCTION_ID_OFFSET] ; lsl x2, x2, #16);
    emit_load_u64(ops, 16, otter_vm::value::tag::box_function_id(0));
    dynasm!(ops ; .arch aarch64 ; orr x2, x2, x16);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        otter_vm::runtime_stubs::CONSTRUCTOR_RECEIVER_PROBE.entry_addr() as u64,
        abi::STUB_CONSTRUCTOR_RECEIVER_PROBE,
    );
    dynasm!(ops ; .arch aarch64 ; blr x16 ; cbz x1, =>hit
        ; cmp x1, #abi::NativeResultStatus::SideExit as u32 ; b.ne =>fatal
        ; cbnz x0, =>fatal ; b =>canonical ; =>hit);
    emit_load_u64(ops, 15, !u64::from(u32::MAX));
    dynasm!(ops ; .arch aarch64 ; and x14, x0, x15);
    emit_load_u64(ops, 15, otter_vm::value::tag::NUMBER_TAG);
    dynasm!(ops ; .arch aarch64 ; cmp x14, x15 ; b.ne =>fatal
        ; mov w9, w0 ; cbz w9, =>fatal);
    emit_fit(ops, relocations, view, space);
    dynasm!(ops ; .arch aarch64 ; mov x1, x0 ; mov x0, x20);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.entry(abi::STUB_CONSTRUCTOR_RECEIVER_COMMIT),
        abi::STUB_CONSTRUCTOR_RECEIVER_COMMIT,
    );
    dynasm!(ops ; .arch aarch64 ; blr x16 ; cbz x1, =>complete
        ; cmp x1, #abi::NativeResultStatus::Fatal as u32 ; b.ne =>fatal);
    emit_load_u64(ops, 15, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; cmp x0, x15 ; b.ne =>fatal ; b =>complete);
    dynasm!(ops ; .arch aarch64 ; =>space);
    dynasm!(ops ; .arch aarch64 ; b =>canonical ; =>fatal);
    emit_load_u64(ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; mov x1, #abi::NativeResultStatus::Fatal as u64 ; b =>complete);
}

/// Validated w9 layout remains owned by Frame.new.target; this extent cannot GC.
fn emit_fit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    space: DynamicLabel,
) {
    assert_eq!(
        view.field_layout.cell_bytes(0),
        view.field_layout.inline_values_byte as usize
    );
    const _: () = assert!(otter_vm::jit::JIT_GC_HEADER_SIZE_BYTES_OFFSET == 4);
    emit_load_symbol_u64(
        ops,
        relocations,
        12,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch aarch64
        ; add x15, x12, x9 ; ldr w10, [x15, view.constructor_layout.root_byte]
        ; add x15, x12, x10 ; ldrb w14, [x15, view.shape_inline_capacity_byte]
        ; lsl x17, x14, #3 ; add x17, x17, #view.field_layout.cell_bytes(0) as u32);
    emit_bump_probe(ops, 20, LAB, space);
    emit_load_u64(ops, 16, otter_vm::jit::ordinary_object_header_word(0));
    dynasm!(ops ; .arch aarch64 ; orr x16, x16, x17, lsl #32 ; str x16, [x0]);
    for byte in (8..view.field_layout.cell_bytes(0) as u32).step_by(8) {
        dynasm!(ops ; .arch aarch64 ; str xzr, [x0, byte]);
    }
    dynasm!(ops ; .arch aarch64 ; str w10, [x0, view.object_shape_byte]
        ; add x12, x0, #view.field_layout.inline_values_byte);
    emit_load_u64(ops, 16, VALUE_UNDEFINED);
    let initialized = ops.new_dynamic_label();
    let fill = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; cmp x12, x13 ; b.eq =>initialized
        ; =>fill ; str x16, [x12], #8 ; cmp x12, x13 ; b.ne =>fill ; =>initialized);
    emit_publish(ops, 20, OBJECT_BODY_TYPE_TAG as u8, LAB);
    count(ops, RECEIVER_ALLOC_GENERATED_OFFSET);
    dynasm!(ops ; .arch aarch64
        ; str w9, [x21, abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET]
        ; str x0, [x21, abi::NATIVE_FRAME_CONSTRUCT_RECEIVER_OFFSET]
        ; str x0, [x21, NATIVE_FRAME_THIS_OFFSET]);
}

#[cfg(test)]
#[path = "../../allocation/receiver_dynamic_tests.rs"]
mod tests;
