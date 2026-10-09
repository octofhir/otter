//! Actual-family receivers in the shared generated callee prefix.
//!
//! # Contents
//! - Existing boxed LeafValue2 admission and ContextWords feedback commit.
//! - Variable-capacity initialization through the common LAB owner.
//!
//! # Invariants
//! Frame owns new.target/actuals/tagged homes before either leaf. Only a nonzero
//! int32 Probe Success reaches writes. No call/GC separates complete payload
//! initialization from one LAB publication. Pre-effect misses prepare once;
//! post-publication failure is Fatal. The one C adapter owns Windows geometry.
//!
//! # See also
//! - `otter_vm::constructor_layout` owns preparation, sampling and feedback.
//! - `super` owns LAB bump, publication and per-type accounting.

use super::{emit_bump_probe, emit_publish};
use crate::x86_64::{
    call_abi::emit_runtime_call,
    values::{emit_load_runtime_stub, emit_load_symbol_u64, emit_load_u64},
};
use crate::{
    allocation::LabRegisters,
    artifact::relocation::{RelocationCapture, RelocationTarget},
    entry::{
        NATIVE_FRAME_THIS_OFFSET, OBJECT_BODY_TYPE_TAG, RECEIVER_ALLOC_ATTEMPTS_OFFSET,
        RECEIVER_ALLOC_GENERATED_OFFSET, RECEIVER_ALLOC_GUARD_MISSES_OFFSET,
        RECEIVER_ALLOC_SPACE_MISSES_OFFSET, RUNTIME_STATS_OFFSET, THREAD_OFFSET,
        TransitionTable, VALUE_UNDEFINED, VM_THREAD_GC_HEAP_OFFSET,
    },
};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, native_abi as abi};

const LAB: LabRegisters = LabRegisters {
    buffer: 6,
    candidate: 0,
    end: 7,
    scratch: 11,
    size: 2,
};
fn count(ops: &mut Assembler, byte: u32) {
    dynasm!(ops ; .arch x64 ; mov r10, [r15 + RUNTIME_STATS_OFFSET as i32]
        ; add QWORD [r10 + byte as i32], 1);
}

/// r15 context/r14 frame/r13 actuals/r12 promotion decision are pinned. Body
/// values are not live yet. Calls use the current platform C adapter.
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
    let refused = ops.new_dynamic_label();
    count(ops, RECEIVER_ALLOC_ATTEMPTS_OFFSET);
    dynasm!(ops ; .arch x64
        ; mov rdi, [r15 + THREAD_OFFSET as i32] ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]
        ; mov rsi, [r14 + abi::NATIVE_FRAME_NEW_TARGET_OFFSET as i32]
        ; mov edx, [r14 + abi::NATIVE_FRAME_FUNCTION_ID_OFFSET as i32] ; shl rdx, 16
        ; or rdx, otter_vm::value::tag::box_function_id(0) as i32);
    emit_load_runtime_stub(
        ops,
        relocations,
        otter_vm::runtime_stubs::CONSTRUCTOR_RECEIVER_PROBE.entry_addr() as u64,
        abi::STUB_CONSTRUCTOR_RECEIVER_PROBE,
    );
    emit_runtime_call(ops, abi::STUB_CONSTRUCTOR_RECEIVER_PROBE);
    dynasm!(ops ; .arch x64 ; test rdx, rdx ; jz =>hit
        ; cmp rdx, abi::NativeResultStatus::SideExit as i32 ; jne =>fatal
        ; test rax, rax ; jnz =>fatal ; jmp =>refused ; =>hit);
    emit_load_u64(ops, 11, !u64::from(u32::MAX));
    dynasm!(ops ; .arch x64 ; mov r10, rax ; and r10, r11);
    emit_load_u64(ops, 11, otter_vm::value::tag::NUMBER_TAG);
    dynasm!(ops ; .arch x64 ; cmp r10, r11 ; jne =>fatal
        ; mov r8d, eax ; test r8d, r8d ; jz =>fatal);
    emit_fit(ops, relocations, view, space);
    dynasm!(ops ; .arch x64 ; mov rsi, rax ; mov rdi, r15);
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.entry(abi::STUB_CONSTRUCTOR_RECEIVER_COMMIT),
        abi::STUB_CONSTRUCTOR_RECEIVER_COMMIT,
    );
    emit_runtime_call(ops, abi::STUB_CONSTRUCTOR_RECEIVER_COMMIT);
    dynasm!(ops ; .arch x64 ; test rdx, rdx ; jz =>complete
        ; cmp rdx, abi::NativeResultStatus::Fatal as i32 ; jne =>fatal
        ; cmp rax, VALUE_UNDEFINED as i32 ; jne =>fatal ; jmp =>complete);
    dynasm!(ops ; .arch x64 ; =>refused);
    count(ops, RECEIVER_ALLOC_GUARD_MISSES_OFFSET);
    dynasm!(ops ; .arch x64 ; jmp =>canonical ; =>space);
    count(ops, RECEIVER_ALLOC_SPACE_MISSES_OFFSET);
    dynasm!(ops ; .arch x64 ; jmp =>canonical ; =>fatal
        ; mov eax, VALUE_UNDEFINED as i32 ; mov edx, abi::NativeResultStatus::Fatal as i32
        ; jmp =>complete);
}

/// r8d is the current layout borrowed under Frame.new.target. It survives
/// the common LAB publication; this helper has no hidden scratch stack.
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
        11,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch x64
        ; lea r10, [r11 + r8] ; mov r9d, [r10 + view.constructor_layout.root_byte as i32]
        ; lea r10, [r11 + r9] ; movzx edx, BYTE [r10 + view.shape_inline_capacity_byte as i32]
        ; shl rdx, 3 ; add rdx, view.field_layout.cell_bytes(0) as i32);
    emit_bump_probe(ops, 15, LAB, space);
    dynasm!(ops ; .arch x64 ; mov r11, rdx ; shl r11, 32
        ; or r11, otter_vm::jit::ordinary_object_header_word(0) as i32 ; mov [rax], r11);
    for byte in (8..view.field_layout.cell_bytes(0) as u32).step_by(8) {
        dynasm!(ops ; .arch x64 ; mov QWORD [rax + byte as i32], 0);
    }
    dynasm!(ops ; .arch x64 ; mov [rax + view.object_shape_byte as i32], r9d
        ; lea r9, [rax + view.field_layout.inline_values_byte as i32]);
    emit_load_u64(ops, 11, VALUE_UNDEFINED);
    let initialized = ops.new_dynamic_label();
    let fill = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; cmp r9, rdi ; je =>initialized
        ; =>fill ; mov [r9], r11 ; add r9, 8 ; cmp r9, rdi ; jne =>fill ; =>initialized);
    emit_publish(ops, 15, OBJECT_BODY_TYPE_TAG as u8, LAB);
    count(ops, RECEIVER_ALLOC_GENERATED_OFFSET);
    dynasm!(ops ; .arch x64
        ; mov [r14 + abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET as i32], r8d
        ; mov [r14 + abi::NATIVE_FRAME_CONSTRUCT_RECEIVER_OFFSET as i32], rax
        ; mov [r14 + NATIVE_FRAME_THIS_OFFSET as i32], rax);
}

#[cfg(test)]
#[path = "../../allocation/receiver_dynamic_tests.rs"]
mod tests;
