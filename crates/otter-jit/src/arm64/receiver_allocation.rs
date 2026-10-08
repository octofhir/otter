//! Shared nursery receiver allocation after live constructor/prototype proofs.
//!
//! # Contents
//! - Class-wrapper and ordinary-closure prototype resolution before effects.
//! - Linear-allocation-buffer bump probe and complete object initialization.
//! - Explicit publication/accounting after every pre-effect proof succeeds.
//! - SSA probe completion with separately counted pre-effect misses.
//!
//! # Invariants
//! - A live exact constructor family must be finalized and match the plan;
//!   first-seven provisional lineages always use canonical preparation.
//! - Descriptor/shape guards load the live own prototype slot, never a cached
//!   prototype value. Every uncertain case reaches rooted canonical preparation.
//! - The receiver's shape fixes its prototype: a baked shape is proved to fix
//!   the live prototype and immutable inline capacity. An empty receiver uses
//!   the exact baked capacity root, never a prototype cache-chain head.
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
//! - `otter_vm::closure_construct` — exact family state and canonical terminal sampling.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, closure::JS_CLOSURE_BODY_TYPE_TAG, value::tag as value_tag};

use crate::arm64::allocation::emit_count_allocation;
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_u64, emit_load_u64_wide};
use crate::{
    artifact::relocation::{RelocationCapture, RelocationTarget},
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
    dynasm!(ops ; .arch aarch64 ; b =>ready);
}

/// Prove the live new.target in `x2` constructs `plan`'s receiver: its exact
/// finalized family, and a live prototype the receiver's shape fixes. Every
/// failure branches to `miss` before any effect. Preserves `x2`; clobbers
/// `x4`, `x11`, `x12`, `x14`, `x15` and `x17`.
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
    let closure = ops.new_dynamic_label();
    let prototype_ready = ops.new_dynamic_label();
    emit_cell_test(ops, 2, CellTest::IsNotCell, guard_miss);
    emit_symbol(
        ops,
        relocations,
        12,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch aarch64
        ; ldrb w11, [x2]
        ; cmp w11, JS_CLOSURE_BODY_TYPE_TAG as u32 ; b.eq =>closure
    );
    if !plan.new_target_is_class {
        dynasm!(ops ; .arch aarch64 ; b =>guard_miss);
    }
    dynasm!(ops ; .arch aarch64
        ; cmp w11, view.class_constructor_layout.type_tag as u32 ; b.ne =>guard_miss
        ; ldr x14, [x2, view.class_constructor_layout.callable_byte]
    );
    emit_load_u64(
        ops,
        11,
        value_tag::box_function_id(plan.new_target_function_id),
    );
    let target_ready = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; cmp x14, x11 ; b.eq =>target_ready);
    emit_cell_test(ops, 14, CellTest::IsNotCell, guard_miss);
    dynasm!(ops ; .arch aarch64
        ; ldrb w11, [x14] ; cmp w11, JS_CLOSURE_BODY_TYPE_TAG as u32 ; b.ne =>guard_miss
        ; ldr w11, [x14, view.closure_call_layout.function_id_byte]
    );
    emit_compare_function_id(ops, plan.new_target_function_id);
    dynasm!(ops ; .arch aarch64
        ; b.ne =>guard_miss ; =>target_ready
        ; ldr w17, [x2, view.class_constructor_layout.constructor_layouts_byte]
    );
    emit_finalized_family_guard(ops, view, plan, guard_miss);
    dynasm!(ops ; .arch aarch64
        ; ldr w4, [x2, view.class_constructor_layout.prototype_byte]
        ; cbz w4, =>guard_miss ; b =>prototype_ready
        ; =>closure
        ; ldr w11, [x2, view.closure_call_layout.function_id_byte]
    );
    emit_compare_function_id(ops, plan.new_target_function_id);
    dynasm!(ops ; .arch aarch64 ; b.ne =>guard_miss);
    // Rare-record and family handles are live traced roots. Neither a
    // receiver nor a moving layout address is baked or weakly observed.
    dynasm!(ops ; .arch aarch64
        ; ldr w17, [x2, view.closure_call_layout.rare_byte]
        ; cbz w17, =>guard_miss ; add x17, x12, x17
        ; ldr w17, [x17, view.closure_call_layout.constructor_layouts_byte]
    );
    emit_finalized_family_guard(ops, view, plan, guard_miss);
    dynasm!(ops ; .arch aarch64
        ; ldr w17, [x2, view.closure_call_layout.rare_byte]
        ; add x17, x12, x17
        ; ldr x4, [x17, view.closure_call_layout.prototype_byte]
    );
    emit_cell_test(ops, 4, CellTest::IsNotCell, guard_miss);
    dynasm!(ops ; .arch aarch64
        ; ldrb w14, [x4] ; cmp w14, OBJECT_BODY_TYPE_TAG ; b.ne =>guard_miss
        ; mov w4, w4
        ; =>prototype_ready
    );
    if let Some(validity) = plan.prototype_validity {
        crate::template::arm64::values::emit_prototype_validity_guard(
            ops,
            relocations,
            validity,
            14,
            guard_miss,
        );
    }
    assert_ne!(
        plan.prototype_root, 0,
        "receiver allocation needs an exact capacity root"
    );
    emit_load_u64(ops, 14, u64::from(plan.prototype_root));
    dynasm!(ops ; .arch aarch64 ; add x14, x12, x14
        ; ldr w14, [x14, view.shape_prototype_byte]
        ; cmp w14, w4 ; b.ne =>guard_miss);
    if plan.receiver_shape != 0 {
        emit_load_u64(ops, 14, u64::from(plan.receiver_shape));
        dynasm!(ops
            ; .arch aarch64
            ; add x14, x12, x14
            ; ldr w14, [x14, view.shape_prototype_byte]
            ; cmp w14, w4
            ; b.ne =>guard_miss
        );
    }
}

/// Initialize `plan`'s receiver at the top of the context's buffer, after
/// [`emit_receiver_guards`] proved it. A fit returns the unpublished cell
/// in `x0` and its buffer in `x1`; no fit branches to `space_miss` before
/// any write. Reads only the context register; clobbers `x0`, `x1`, `x4`
/// and `x13`–`x16`.
pub(crate) fn emit_receiver_fit(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
    context_register: u8,
    space_miss: DynamicLabel,
) {
    let shape = if plan.receiver_shape != 0 {
        plan.receiver_shape
    } else {
        plan.prototype_root
    };
    emit_load_u64(ops, 4, u64::from(shape));

    // Only a complete cell fit below the buffer limit may mutate the nursery.
    // `x13` keeps the buffer for publication; the candidate lives at `top`.
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
    emit_load_u64(ops, 14, plan.cell_header_word(cell_bytes));
    dynasm!(ops
        ; .arch aarch64
        ; str x14, [x16]
        ; str x4, [x16, view.object_shape_byte]
        ; str xzr, [x16, view.object_exotic_handle_byte]
    );
    if plan.inline_capacity != 0 {
        emit_load_u64(ops, 14, VALUE_UNDEFINED);
        for index in 0..u32::from(plan.inline_capacity) {
            let offset = view
                .field_layout
                .inline_byte(otter_vm::object::FieldLocation::inline(index));
            dynasm!(ops ; .arch aarch64 ; str x14, [x16, offset]);
        }
    }
    dynasm!(ops ; .arch aarch64 ; mov x0, x16 ; mov x1, x13);
}

/// Read the head in w17, preserving new.target and the cage base. All
/// failures precede candidate writes; publication never mutates family state.
///
/// A finalized family takes no further terminal samples, so the entered
/// constructor owes no completion ticket and none is published.
fn emit_finalized_family_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
    miss: DynamicLabel,
) {
    let layout = view.constructor_layout;
    dynasm!(ops ; .arch aarch64
        ; cbz w17, =>miss ; add x17, x12, x17
        ; ldr x11, [x17, layout.family_id_byte]
    );
    emit_load_u64(ops, 14, plan.family_id);
    dynasm!(ops ; .arch aarch64
        ; cmp x11, x14 ; b.ne =>miss
        ; ldrb w11, [x17, layout.samples_remaining_byte] ; cbnz w11, =>miss
        ; ldr w11, [x17, layout.root_byte]
    );
    emit_load_u64(ops, 14, u64::from(plan.prototype_root));
    dynasm!(ops ; .arch aarch64 ; cmp w11, w14 ; b.ne =>miss);
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

/// Publish one completely initialized candidate from `x0` in buffer `x1`.
///
/// This operation cannot miss: the buffer bump was proven by
/// `emit_receiver_candidate`, the heap charged the buffer to any cap when it
/// was carved, and generated Machine code has no safepoint or reentry between
/// candidate creation and this effect. The cell size is read back from the
/// candidate's header, which the candidate wrote from its plan. Clobbers
/// `x13`–`x17`.
fn emit_receiver_publication(ops: &mut Assembler, context_register: u8) {
    dynasm!(ops ; .arch aarch64
        ; str x0, [X(context_register), crate::entry::PENDING_CALL_OFFSET + otter_vm::native_abi::REQUEST_CONSTRUCT_RECEIVER_OFFSET]
    );
    emit_receiver_bump(ops, context_register);
    emit_increment_runtime_counter(ops, context_register, RECEIVER_ALLOC_GENERATED_OFFSET);
}

/// Publish the fit in `x0`/`x1` by bumping its buffer and accounting the
/// cell; the receiver stays in `x0`. Clobbers `x13`–`x17`.
pub(crate) fn emit_receiver_bump(ops: &mut Assembler, context_register: u8) {
    dynasm!(ops
        ; .arch aarch64
        ; mov x16, x0
        ; mov x13, x1
        ; ldr w17, [x16, GC_HEADER_SIZE_BYTE]
        ; add x15, x16, x17
        ; str x15, [x13, LAB_TOP_OFFSET]
    );
    emit_count_allocation(
        ops,
        context_register,
        OBJECT_BODY_TYPE_TAG as u8,
        17,
        13,
        14,
    );
    dynasm!(ops ; .arch aarch64 ; mov x0, x16);
}

/// Byte offset of the `u32` cell size inside a GC header.
const GC_HEADER_SIZE_BYTE: u32 = otter_vm::jit::JIT_GC_HEADER_SIZE_BYTES_OFFSET;

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
    _view: &JitCompileSnapshot,
    context_register: u8,
) {
    emit_receiver_publication(ops, context_register);
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

fn emit_symbol(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    emit_load_u64_wide(ops, register, value);
    relocations.record_mov_wide(start, ops.offset().0, register, target);
}

/// Compare `w11` against a baked function id.
///
/// AArch64's `cmp` carries a 12-bit unsigned immediate; a wider id is
/// materialised into `w15` first.
fn emit_compare_function_id(ops: &mut Assembler, function_id: u32) {
    if function_id <= 0xfff {
        dynasm!(ops ; .arch aarch64 ; cmp w11, function_id);
        return;
    }
    dynasm!(ops
        ; .arch aarch64
        ; movz w15, function_id & 0xffff
        ; movk w15, function_id >> 16, lsl 16
        ; cmp w11, w15
    );
}
