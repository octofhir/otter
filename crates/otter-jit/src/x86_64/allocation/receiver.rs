//! Generated constructor receivers on the shared x86 nursery buffer.
//!
//! # Contents
//! - Exact finalized family identity proof of the closure/class new.target.
//! - Complete unpublished receiver initialization and explicit publication.
//!
//! # Invariants
//! - The head of the new.target's family list must be the plan's family. A
//!   plan names only a finalized family, whose root never changes again, and
//!   a family belongs to exactly one constructor object.
//! - A closure's `prototype` store clears its family head and a class's
//!   `prototype` never changes: the identity proves the live prototype the
//!   receiver's shape fixes, so no prototype slot is read.
//! - A miss hands receiver preparation to the entered callee exactly once.
//! - Header, null metadata and every inline slot precede one LAB publication;
//!   no call, collection or reentry separates the candidate and publication.
//!   The candidate lives in R9 until publication: RAX may hold a value a
//!   miss's frame state reads.
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
use otter_vm::{JitCompileSnapshot, jit::JitReceiverAllocationPlan};

const RECEIVER_LAB: LabRegisters = LabRegisters {
    buffer: 2,
    candidate: 9,
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
    let guard_miss = ops.new_dynamic_label();
    let space_miss = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    crate::x86_64::js_call::emit_clear_construct_ticket(ops, context);
    increment_counter(ops, context, RECEIVER_ALLOC_ATTEMPTS_OFFSET);
    emit_receiver_guards(ops, relocations, view, plan, guard_miss);
    emit_receiver_fit(ops, view, plan, context, space_miss);
    dynasm!(ops ; .arch x64 ; mov rax, r9 ; jmp =>ready ; =>guard_miss);
    increment_counter(ops, context, RECEIVER_ALLOC_GUARD_MISSES_OFFSET);
    dynasm!(ops ; .arch x64 ; jmp =>miss ; =>space_miss);
    increment_counter(ops, context, RECEIVER_ALLOC_SPACE_MISSES_OFFSET);
    dynasm!(ops ; .arch x64 ; =>miss);
    crate::x86_64::js_call::emit_clear_construct_ticket(ops, context);
    dynasm!(ops ; .arch x64 ; mov eax, VALUE_UNDEFINED as i32 ; xor edx, edx ; =>ready);
}

/// Prove the new.target in RCX constructs `plan`'s receiver. The caller has
/// proved RCX is the closure of `plan.new_target_function_id` or that
/// function's boxed id; a class plan also admits any value. The head of the
/// owner's family list is the plan's exact family, which proves the live
/// prototype the receiver's shape fixes. Every failure branches to
/// `guard_miss` before any effect. Preserves RCX/RSI; clobbers R8, R10, R11
/// and flags.
pub(crate) fn emit_receiver_guards(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    plan: JitReceiverAllocationPlan,
    guard_miss: DynamicLabel,
) {
    assert_ne!(
        plan.family_id, 0,
        "receiver requires an exact finalized family"
    );
    assert_ne!(
        plan.prototype_root, 0,
        "receiver needs an exact capacity root"
    );
    // A boxed function id owns no family.
    cell_test(ops, 1, guard_miss);
    emit_load_symbol_u64(
        ops,
        relocations,
        11,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    if plan.new_target_is_class {
        let class = view.class_constructor_layout;
        dynasm!(ops ; .arch x64
            ; cmp BYTE [rcx], class.type_tag as i8 ; jne =>guard_miss
            ; mov r8d, [rcx + class.constructor_layouts_byte as i32]
        );
    } else {
        let closure = view.closure_call_layout;
        dynasm!(ops ; .arch x64
            ; mov r8d, [rcx + closure.rare_byte as i32]
            ; test r8d, r8d ; jz =>guard_miss
            ; add r8, r11
            ; mov r8d, [r8 + closure.constructor_layouts_byte as i32]
        );
    }
    let family = view.constructor_layout.family_id_byte as i32;
    dynasm!(ops ; .arch x64 ; test r8d, r8d ; jz =>guard_miss ; add r8, r11);
    match i32::try_from(plan.family_id) {
        Ok(id) => dynasm!(ops ; .arch x64 ; cmp QWORD [r8 + family], id),
        Err(_) => {
            emit_load_u64(ops, 10, plan.family_id);
            dynasm!(ops ; .arch x64 ; cmp [r8 + family], r10);
        }
    }
    dynasm!(ops ; .arch x64 ; jne =>guard_miss);
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
}

/// Initialize `plan`'s receiver at the top of the context's buffer, after
/// [`emit_receiver_guards`] proved it. A fit returns the unpublished cell in
/// R9, its LAB in RDX, its end in RDI and its size in R8; no fit branches to
/// `space_miss` before any write. Reads only `context`; clobbers RDX, RDI,
/// R8–R10 and flags, never RAX.
pub(crate) fn emit_receiver_fit(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: JitReceiverAllocationPlan,
    context: u8,
    space_miss: DynamicLabel,
) {
    let shape = if plan.receiver_shape == 0 {
        plan.prototype_root
    } else {
        plan.receiver_shape
    };
    let bytes = view
        .field_layout
        .cell_bytes(usize::from(plan.inline_capacity)) as u32;
    dynasm!(ops ; .arch x64 ; mov r8d, bytes as i32);
    emit_bump_probe(ops, context, RECEIVER_LAB, space_miss);
    emit_load_u64(ops, 10, plan.cell_header_word(bytes));
    dynasm!(ops ; .arch x64 ; mov [r9], r10);
    let slots = (0..u32::from(plan.inline_capacity))
        .map(|index| {
            view.field_layout
                .inline_byte(otter_vm::object::FieldLocation::inline(index))
        })
        .collect::<Vec<_>>();
    // The shape word holds the shape and a null slab; every other body word
    // (the null sidecar and padding) is zero.
    emit_load_u64(ops, 10, VALUE_UNDEFINED);
    for byte in (8..bytes).step_by(8) {
        if byte == view.object_shape_byte {
            dynasm!(ops ; .arch x64
                ; mov DWORD [r9 + byte as i32], shape as i32
                ; mov DWORD [r9 + byte as i32 + 4], 0
            );
        } else if slots.contains(&byte) {
            dynasm!(ops ; .arch x64 ; mov [r9 + byte as i32], r10);
        } else {
            dynasm!(ops ; .arch x64 ; mov QWORD [r9 + byte as i32], 0);
        }
    }
}

/// Publish the dominated fit from [`emit_receiver_candidate_probe`]. The
/// candidate, LAB, end and size registers still hold the same recipe. No
/// intervening operation may collect or clobber those values. RAX holds the
/// receiver.
pub(crate) fn emit_receiver_publication_effect(
    ops: &mut Assembler,
    _view: &JitCompileSnapshot,
    context: u8,
) {
    dynasm!(ops ; .arch x64
        ; mov [Rq(context) + (crate::entry::PENDING_CALL_OFFSET + otter_vm::native_abi::REQUEST_CONSTRUCT_RECEIVER_OFFSET) as i32], rax
    );
    emit_receiver_bump(ops, context);
    increment_counter(ops, context, RECEIVER_ALLOC_GENERATED_OFFSET);
}

/// Publish the dominated fit from [`emit_receiver_fit`], whose cell, LAB,
/// end and size are still in R9, RDX, RDI and R8, and account the cell; the
/// receiver is returned in RAX. Clobbers RDX and flags.
pub(crate) fn emit_receiver_bump(ops: &mut Assembler, context: u8) {
    emit_publish(ops, context, OBJECT_BODY_TYPE_TAG as u8, RECEIVER_LAB);
    dynasm!(ops ; .arch x64 ; mov rax, r9);
}

#[cfg(test)]
#[path = "receiver/tests.rs"]
mod tests;
