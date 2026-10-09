//! Shared nursery receiver allocation after an exact constructor-family proof.
//!
//! # Contents
//! - Class-wrapper and ordinary-closure family identity before effects.
//! - Linear-allocation-buffer bump probe and complete object initialization.
//! - Explicit publication/accounting after every pre-effect proof succeeds.
//! - SSA probe completion with separately counted pre-effect misses.
//!
//! # Invariants
//! - The head of the new.target's family list must be the plan's family. A
//!   plan names only a finalized family, whose root never changes again, and
//!   a family belongs to exactly one constructor object.
//! - A closure's `prototype` store clears its family head and a class's
//!   `prototype` never changes (V8's initial-map dependency): the identity
//!   proves the live prototype the receiver's shape fixes, so no prototype
//!   slot is read. Every uncertain case reaches rooted canonical preparation.
//! - The receiver's shape fixes its prototype and immutable inline capacity.
//!   An empty receiver uses the exact baked capacity root, never a prototype
//!   cache-chain head.
//! - Header and slots are initialized before publishing the bump. The already
//!   proven family and original receiver enter the sole CallRequest ticket,
//!   consumed by the callee before GC or by the unentered failure owner.
//! - The candidate helper mutates only bytes at or beyond the buffer's `top`;
//!   the publication helper is the first operation that makes the cell
//!   observable.
//! - The heap empties its buffer whenever marking, stress, tenuring or a heap
//!   cap needs the rooted path, so the bump is the only collector test.
//!
//! # See also
//! - `otter_vm::constructor_layout` — exact family state and canonical terminal sampling.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;

use crate::arm64::allocation::emit_count_allocation_bytes;
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_u64};
use crate::{
    artifact::relocation::RelocationCapture,
    entry::{
        ALLOC_WINDOW_LAB_OFFSET, LAB_LIMIT_OFFSET, LAB_TOP_OFFSET, OBJECT_BODY_TYPE_TAG,
        RECEIVER_ALLOC_ATTEMPTS_OFFSET, RECEIVER_ALLOC_GENERATED_OFFSET,
        RECEIVER_ALLOC_GUARD_MISSES_OFFSET, RECEIVER_ALLOC_SPACE_MISSES_OFFSET,
        RUNTIME_STATS_OFFSET, VALUE_UNDEFINED,
    },
};

#[allow(clippy::too_many_arguments)]
fn emit_receiver_candidate(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
    context_register: u8,
    guard_miss: DynamicLabel,
    space_miss: DynamicLabel,
    ready: DynamicLabel,
) {
    crate::arm64::js_call::emit_clear_construct_ticket(ops, context_register);
    emit_increment_runtime_counter(ops, context_register, RECEIVER_ALLOC_ATTEMPTS_OFFSET);
    emit_receiver_guards(ops, relocations, view, plan, guard_miss);
    emit_receiver_fit(ops, view, plan, context_register, space_miss);
    dynasm!(ops ; .arch aarch64 ; mov x0, x16 ; mov x1, x13 ; b =>ready);
}

/// Prove the new.target in `x2` constructs `plan`'s receiver. The caller has
/// proved `x2` is the closure of `plan.new_target_function_id` or that
/// function's boxed id; a class plan also admits any value. The head of the
/// owner's family list is the plan's exact family: a family is owned by one
/// constructor object, stays finalized with its baked root once finalized,
/// and a `prototype` change clears the head, so the identity alone proves
/// the live prototype the receiver's shape fixes. Every failure branches to
/// `guard_miss` before any effect. Preserves `x2`; clobbers `x11`, `x12`,
/// `x14` and `x17`.
pub(crate) fn emit_receiver_guards(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
    guard_miss: DynamicLabel,
) {
    assert_ne!(
        plan.family_id, 0,
        "receiver requires an exact finalized family"
    );
    assert_ne!(
        plan.prototype_root, 0,
        "receiver allocation needs an exact capacity root"
    );
    // A boxed function id owns no family; a cell's cage is its high half.
    emit_cell_test(ops, 2, CellTest::IsNotCell, guard_miss);
    dynasm!(ops ; .arch aarch64 ; and x12, x2, #0xffff_ffff_0000_0000);
    if plan.new_target_is_class {
        let class = view.class_constructor_layout;
        dynasm!(ops ; .arch aarch64
            ; ldrb w11, [x2] ; cmp w11, class.type_tag as u32 ; b.ne =>guard_miss
            ; ldr w17, [x2, class.constructor_layouts_byte]
        );
    } else {
        let closure = view.closure_call_layout;
        dynasm!(ops ; .arch aarch64
            ; ldr w17, [x2, closure.rare_byte]
            ; cbz w17, =>guard_miss ; add x17, x12, x17
            ; ldr w17, [x17, closure.constructor_layouts_byte]
        );
    }
    dynasm!(ops ; .arch aarch64
        ; cbz w17, =>guard_miss ; add x17, x12, x17
        ; ldr x11, [x17, view.constructor_layout.family_id_byte]
    );
    if plan.family_id <= 0xfff {
        dynasm!(ops ; .arch aarch64 ; cmp x11, plan.family_id as u32);
    } else {
        emit_load_u64(ops, 14, plan.family_id);
        dynasm!(ops ; .arch aarch64 ; cmp x11, x14);
    }
    dynasm!(ops ; .arch aarch64 ; b.ne =>guard_miss);
    if let Some(validity) = plan.prototype_validity {
        crate::template::arm64::values::emit_prototype_validity_guard(
            ops,
            relocations,
            validity,
            14,
            guard_miss,
        );
    }
}

/// Initialize `plan`'s receiver at the top of the context's buffer, after
/// [`emit_receiver_guards`] proved it. A fit returns the unpublished cell in
/// `x16`, its buffer in `x13` and the cell's end in `x15`; no fit branches to
/// `space_miss` before any write, and `x0` is never written: it may hold a
/// value the miss's frame state reads. Reads only the context register;
/// clobbers `x4` and `x13`–`x17`.
pub(crate) fn emit_receiver_fit(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
    context_register: u8,
    space_miss: DynamicLabel,
) {
    // Only a complete cell fit below the buffer limit may mutate the nursery.
    let cell_bytes = receiver_cell_bytes(view, plan);
    dynasm!(ops
        ; .arch aarch64
        ; ldr x13, [X(context_register), ALLOC_WINDOW_LAB_OFFSET]
        ; ldr x16, [x13, LAB_TOP_OFFSET]
        ; ldr x14, [x13, LAB_LIMIT_OFFSET]
        ; add x15, x16, cell_bytes
        ; cmp x15, x14
        ; b.hi =>space_miss
    );

    // Initialize the header and the whole fixed body before publishing the
    // bump cursor. The header carries flags; capacity belongs to the shape;
    // the body is two words: shape + null slab, null sidecar + padding.
    // The receiver shape names exactly the initial fields, so the slot count
    // follows from it. In-object words past the initial fields are
    // all initialized to undefined before a store publishes them.
    let shape = if plan.receiver_shape != 0 {
        plan.receiver_shape
    } else {
        plan.prototype_root
    };
    emit_load_u64(ops, 14, plan.cell_header_word(cell_bytes));
    emit_load_u64(ops, 4, u64::from(shape));
    if plan.inline_capacity != 0 {
        emit_load_u64(ops, 17, VALUE_UNDEFINED);
    }
    let slots = (0..u32::from(plan.inline_capacity))
        .map(|index| {
            view.field_layout
                .inline_byte(otter_vm::object::FieldLocation::inline(index))
        })
        .collect::<Vec<_>>();
    debug_assert_eq!(view.object_shape_byte % 8, 0);
    // Every other body word (the null sidecar and padding) is zero.
    let words = (0..cell_bytes)
        .step_by(8)
        .map(|byte| match byte {
            0 => (byte, 14),
            byte if byte == view.object_shape_byte => (byte, 4),
            byte if slots.contains(&byte) => (byte, 17),
            byte => (byte, 31),
        })
        .collect::<Vec<_>>();
    emit_store_words(ops, 16, &words);
}

/// Store each `(byte, register)` word, in ascending byte order, into the
/// cell at `X(cell)`, pairing adjacent words. Register 31 stores zero.
fn emit_store_words(ops: &mut Assembler, cell: u8, words: &[(u32, u8)]) {
    let mut index = 0;
    while index < words.len() {
        let (byte, first) = words[index];
        match words.get(index + 1) {
            Some(&(next, second)) if next == byte + 8 && byte <= 504 => {
                dynasm!(ops ; .arch aarch64 ; stp X(first), X(second), [X(cell), byte as i32]);
                index += 2;
            }
            _ => {
                dynasm!(ops ; .arch aarch64 ; str X(first), [X(cell), byte]);
                index += 1;
            }
        }
    }
}

/// Cell bytes of the receiver `plan` allocates: the fixed object cell plus
/// its in-object slots.
fn receiver_cell_bytes(
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
) -> u32 {
    view.field_layout
        .cell_bytes(usize::from(plan.inline_capacity)) as u32
}

/// Publish one completely initialized candidate fit, also in `x0`.
///
/// This operation cannot miss: the buffer bump was proven by
/// `emit_receiver_candidate`, the heap charged the buffer to any cap when it
/// was carved, and generated Machine code has no safepoint or reentry between
/// candidate creation and this effect. Clobbers `x13`–`x15`.
fn emit_receiver_publication(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
    context_register: u8,
) {
    dynasm!(ops ; .arch aarch64
        ; str x0, [X(context_register), crate::entry::PENDING_CALL_OFFSET + otter_vm::native_abi::REQUEST_CONSTRUCT_RECEIVER_OFFSET]
    );
    emit_receiver_bump(ops, view, plan, context_register);
    emit_increment_runtime_counter(ops, context_register, RECEIVER_ALLOC_GENERATED_OFFSET);
}

/// Publish the fit of `plan` — cell in `x16`, buffer in `x13`, end in `x15`
/// — by bumping its buffer and accounting the cell; the receiver is returned
/// in `x0`. Clobbers `x13`–`x15`.
pub(crate) fn emit_receiver_bump(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
    context_register: u8,
) {
    dynasm!(ops ; .arch aarch64 ; str x15, [x13, LAB_TOP_OFFSET]);
    emit_count_allocation_bytes(
        ops,
        context_register,
        OBJECT_BODY_TYPE_TAG as u8,
        receiver_cell_bytes(view, plan),
        13,
        14,
    );
    dynasm!(ops ; .arch aarch64 ; mov x0, x16);
}

/// Complete the allocation half of an optimizing receiver probe.
///
/// `x2` is the new.target input. A hit returns the initialized unpublished cell
/// in `x0` and its allocation buffer in `x1`; either pre-effect miss returns
/// undefined in `x0` and zero in `x1`.
pub(crate) fn emit_receiver_candidate_probe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
    context_register: u8,
) {
    let guard_miss = ops.new_dynamic_label();
    let space_miss = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    emit_receiver_candidate(
        ops,
        relocations,
        view,
        plan,
        context_register,
        guard_miss,
        space_miss,
        ready,
    );
    dynasm!(ops ; .arch aarch64 ; =>guard_miss);
    emit_increment_runtime_counter(ops, context_register, RECEIVER_ALLOC_GUARD_MISSES_OFFSET);
    dynasm!(ops ; .arch aarch64 ; b =>miss ; =>space_miss);
    emit_increment_runtime_counter(ops, context_register, RECEIVER_ALLOC_SPACE_MISSES_OFFSET);
    dynasm!(ops ; .arch aarch64 ; =>miss);
    crate::arm64::js_call::emit_clear_construct_ticket(ops, context_register);
    emit_load_u64(ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; mov x1, xzr);
    dynasm!(ops ; .arch aarch64 ; =>ready);
}

/// Commit one optimizing receiver candidate after the probe's hit edge
/// dominates.
pub(crate) fn emit_receiver_publication_effect(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
    context_register: u8,
) {
    emit_receiver_publication(ops, view, plan, context_register);
}

fn emit_increment_runtime_counter(ops: &mut Assembler, context_register: u8, offset: u32) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x15, [X(context_register), RUNTIME_STATS_OFFSET]
        ; ldr x14, [x15, offset]
        ; add x14, x14, #1
        ; str x14, [x15, offset]
    );
}
