//! Generated constructor receivers on the shared x86 nursery buffer.
//!
//! # Contents
//! - Live closure/class prototype and exact finalized family proofs.
//! - Complete unpublished receiver initialization and explicit publication.
//!
//! # Invariants
//! - A live exact constructor family must be finalized and match the plan;
//!   first-seven provisional lineages always use canonical preparation.
//! - The exact immutable capacity root and optional initial shape both fix
//!   the live prototype. No cached prototype value substitutes for that read.
//! - A miss hands receiver preparation to the entered callee exactly once.
//! - Header, null metadata and every inline slot precede one LAB publication;
//!   no call, collection or reentry separates the candidate and publication.
//! - RSI and RCX retain the callee and new.target on every path. The caller
//!   stages actuals before this probe and has already spilled live call values.
//! - The checked family and committed receiver transfer through the one incoming
//!   CallRequest ticket; callee admission failure completes that ticket locally.
//!
//! # See also
//! - [`super`] owns the one x86 LAB probe and publication encoder.
//! - `otter_vm::constructor_layout` owns exact family state and terminal sampling.
//! - `crate::arm64::receiver_allocation` is the peer instruction encoder.

use super::{emit_bump_probe, emit_publish};
use crate::allocation::LabRegisters;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    OBJECT_BODY_TYPE_TAG, RECEIVER_ALLOC_ATTEMPTS_OFFSET, RECEIVER_ALLOC_GENERATED_OFFSET,
    RECEIVER_ALLOC_GUARD_MISSES_OFFSET, RECEIVER_ALLOC_SPACE_MISSES_OFFSET, RUNTIME_STATS_OFFSET,
    VALUE_UNDEFINED,
};
use crate::x86_64::values::{emit_load_symbol_u64, emit_load_u64};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{
    JitCompileSnapshot, closure::JS_CLOSURE_BODY_TYPE_TAG, jit::JitReceiverAllocationPlan,
};

const RECEIVER_LAB: LabRegisters = LabRegisters {
    buffer: 2,
    candidate: 0,
    end: 7,
    scratch: 10,
    size: 8,
};

fn increment_counter(ops: &mut Assembler, context: u8, byte: u32) {
    dynasm!(ops ; .arch x64
        ; mov r10, [Rq(context) + RUNTIME_STATS_OFFSET as i32]
        ; add QWORD [r10 + byte as i32], 1
    );
}

fn cell_test(ops: &mut Assembler, value: u8, miss: DynamicLabel) {
    emit_load_u64(ops, 10, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch x64
        ; test Rq(value), Rq(value) ; jz =>miss
        ; test Rq(value), r10 ; jnz =>miss
    );
}

/// The current head is in r8d; r11 keeps the cage base. Full-word identity
/// never aliases sibling closures or a replacement prototype family. A
/// finalized family takes no further terminal samples, so the entered
/// constructor owes no completion ticket and none is published.
fn finalized_family_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: JitReceiverAllocationPlan,
    miss: DynamicLabel,
) {
    let layout = view.constructor_layout;
    dynasm!(ops ; .arch x64 ; test r8d, r8d ; jz =>miss ; add r8, r11);
    emit_load_u64(ops, 10, plan.family_id);
    dynasm!(ops ; .arch x64
        ; cmp [r8 + layout.family_id_byte as i32], r10 ; jne =>miss
        ; cmp BYTE [r8 + layout.samples_remaining_byte as i32], 0 ; jne =>miss
        ; cmp DWORD [r8 + layout.root_byte as i32], plan.prototype_root as i32 ; jne =>miss
    );
}

/// Probe the constructor in RCX. A fit returns the initialized unpublished
/// receiver in RAX and its LAB in RDX; a miss returns undefined/zero.
/// Clobbers RAX/RDX/RDI/R8–R11 and flags, preserving RSI/RCX, nonvolatiles
/// and all floating registers. `context` must be a pinned nonvolatile.
pub(crate) fn emit_receiver_candidate_probe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    plan: JitReceiverAllocationPlan,
    context: u8,
) {
    debug_assert!(![0, 1, 2, 6, 7, 8, 9, 10, 11].contains(&context));
    assert_ne!(
        plan.family_id, 0,
        "receiver requires an exact finalized family"
    );
    assert_ne!(
        plan.prototype_root, 0,
        "receiver needs an exact capacity root"
    );
    let guard_miss = ops.new_dynamic_label();
    let space_miss = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    let closure = ops.new_dynamic_label();
    let prototype_ready = ops.new_dynamic_label();
    crate::x86_64::js_call::emit_clear_construct_ticket(ops, context);
    increment_counter(ops, context, RECEIVER_ALLOC_ATTEMPTS_OFFSET);
    cell_test(ops, 1, guard_miss);
    emit_load_symbol_u64(
        ops,
        relocations,
        11,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch x64
        ; cmp BYTE [rcx], JS_CLOSURE_BODY_TYPE_TAG as i8 ; je =>closure
    );
    if !plan.new_target_is_class {
        dynasm!(ops ; .arch x64 ; jmp =>guard_miss);
    }
    dynasm!(ops ; .arch x64
        ; cmp BYTE [rcx], view.class_constructor_layout.type_tag as i8 ; jne =>guard_miss
        ; mov r8, [rcx + view.class_constructor_layout.callable_byte as i32]
    );
    emit_load_u64(
        ops,
        0,
        otter_vm::value::tag::box_function_id(plan.new_target_function_id),
    );
    let target_ready = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; cmp r8, rax ; je =>target_ready);
    cell_test(ops, 8, guard_miss);
    dynasm!(ops ; .arch x64
        ; cmp BYTE [r8], JS_CLOSURE_BODY_TYPE_TAG as i8 ; jne =>guard_miss
        ; cmp DWORD [r8 + view.closure_call_layout.function_id_byte as i32], plan.new_target_function_id as i32
        ; jne =>guard_miss
        ; =>target_ready
        ; mov r8d, [rcx + view.class_constructor_layout.constructor_layouts_byte as i32]
    );
    finalized_family_guard(ops, view, plan, guard_miss);
    dynasm!(ops ; .arch x64
        ; mov edx, [rcx + view.class_constructor_layout.prototype_byte as i32]
        ; test edx, edx ; jz =>guard_miss
        ; jmp =>prototype_ready
        ; =>closure
        ; cmp DWORD [rcx + view.closure_call_layout.function_id_byte as i32], plan.new_target_function_id as i32
        ; jne =>guard_miss
        ; mov r8d, [rcx + view.closure_call_layout.rare_byte as i32]
        ; test r8d, r8d ; jz =>guard_miss
        ; add r8, r11
        ; mov r8d, [r8 + view.closure_call_layout.constructor_layouts_byte as i32]
    );
    finalized_family_guard(ops, view, plan, guard_miss);
    dynasm!(ops ; .arch x64
        ; mov r8d, [rcx + view.closure_call_layout.rare_byte as i32]
        ; add r8, r11
        ; mov rdx, [r8 + view.closure_call_layout.prototype_byte as i32]
    );
    cell_test(ops, 2, guard_miss);
    dynasm!(ops ; .arch x64
        ; cmp BYTE [rdx], OBJECT_BODY_TYPE_TAG as i8 ; jne =>guard_miss
        ; mov edx, edx
        ; =>prototype_ready
    );
    if let Some(validity) = plan.prototype_validity {
        emit_load_symbol_u64(
            ops,
            relocations,
            10,
            validity.address as u64,
            RelocationTarget::PrototypeValidityCell {
                identity: validity.identity,
            },
        );
        // Ordinary x86 loads provide the required acquire ordering.
        dynasm!(ops ; .arch x64 ; cmp DWORD [r10], 0 ; je =>guard_miss);
    }
    dynasm!(ops ; .arch x64
        ; mov eax, plan.prototype_root as i32
        ; add rax, r11
        ; cmp [rax + view.shape_prototype_byte as i32], edx ; jne =>guard_miss
    );
    if plan.receiver_shape != 0 {
        dynasm!(ops ; .arch x64
            ; mov eax, plan.receiver_shape as i32 ; add rax, r11
            ; cmp [rax + view.shape_prototype_byte as i32], edx ; jne =>guard_miss
        );
    }
    let shape = if plan.receiver_shape == 0 {
        plan.prototype_root
    } else {
        plan.receiver_shape
    };
    let bytes = view
        .field_layout
        .cell_bytes(usize::from(plan.inline_capacity)) as u32;
    dynasm!(ops ; .arch x64 ; mov r9d, shape as i32 ; mov r8d, bytes as i32);
    emit_bump_probe(ops, context, RECEIVER_LAB, space_miss);
    emit_load_u64(ops, 10, plan.cell_header_word(bytes));
    dynasm!(ops ; .arch x64 ; mov [rax], r10);
    for byte in (8..bytes).step_by(8) {
        dynasm!(ops ; .arch x64 ; mov QWORD [rax + byte as i32], 0);
    }
    dynasm!(ops ; .arch x64 ; mov DWORD [rax + view.object_shape_byte as i32], r9d);
    emit_load_u64(ops, 10, VALUE_UNDEFINED);
    for index in 0..u32::from(plan.inline_capacity) {
        let byte = view
            .field_layout
            .inline_byte(otter_vm::object::FieldLocation::inline(index));
        dynasm!(ops ; .arch x64 ; mov [rax + byte as i32], r10);
    }
    dynasm!(ops ; .arch x64 ; jmp =>ready ; =>guard_miss);
    increment_counter(ops, context, RECEIVER_ALLOC_GUARD_MISSES_OFFSET);
    dynasm!(ops ; .arch x64 ; jmp =>miss ; =>space_miss);
    increment_counter(ops, context, RECEIVER_ALLOC_SPACE_MISSES_OFFSET);
    dynasm!(ops ; .arch x64 ; =>miss);
    crate::x86_64::js_call::emit_clear_construct_ticket(ops, context);
    dynasm!(ops ; .arch x64 ; mov eax, VALUE_UNDEFINED as i32 ; xor edx, edx ; =>ready);
}

/// Publish the dominated fit from [`emit_receiver_candidate_probe`]. The
/// candidate, LAB, end and size registers still hold the same recipe. No
/// intervening operation may collect or clobber those values. RAX survives.
pub(crate) fn emit_receiver_publication_effect(
    ops: &mut Assembler,
    _view: &JitCompileSnapshot,
    context: u8,
) {
    dynasm!(ops ; .arch x64
        ; mov [Rq(context) + (crate::entry::PENDING_CALL_OFFSET + otter_vm::native_abi::REQUEST_CONSTRUCT_RECEIVER_OFFSET) as i32], rax
    );
    emit_publish(ops, context, OBJECT_BODY_TYPE_TAG as u8, RECEIVER_LAB);
    increment_counter(ops, context, RECEIVER_ALLOC_GENERATED_OFFSET);
}

#[cfg(test)]
#[path = "receiver/tests.rs"]
mod tests;
