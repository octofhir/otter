//! Shared nursery receiver allocation after live constructor/prototype proofs.
//!
//! # Contents
//! - Class-wrapper and ordinary-closure prototype resolution before effects.
//! - Linear-allocation-buffer bump probe and complete object initialization.
//! - Explicit publication/accounting after every pre-effect proof succeeds.
//! - SSA probe completion with separately counted pre-effect misses.
//!
//! # Invariants
//! - Ordinary closure probes require an active weak-observation ledger entry;
//!   GC flush invalidates that permission before moving either sampled object.
//! - Descriptor/shape guards load the live own prototype slot, never a cached
//!   prototype value. Every uncertain case reaches rooted canonical preparation.
//! - Header, slots and prototype are initialized before publishing the bump.
//! - The candidate helper mutates only bytes at or beyond the buffer's `top`;
//!   the publication helper is the first operation that makes the cell
//!   observable.
//! - The heap empties its buffer whenever marking, stress, tenuring or a heap
//!   cap needs the rooted path, so the bump is the only collector test.
//!
//! # See also
//! - `otter_vm::closure_construct` — canonical state and weak observation lifetime.

use super::*;
use crate::template::arm64::values::{CellTest, emit_cell_test};

/// Emit the shared no-safepoint receiver allocation program.
///
/// `new.target` is in `x2`. A hit returns the complete object value in `x0`;
/// guard and nursery misses branch separately so their counters remain exact.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_generated_receiver_allocation(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
    context_register: u8,
    guard_miss: DynamicLabel,
    space_miss: DynamicLabel,
    ready: DynamicLabel,
) {
    let candidate_ready = ops.new_dynamic_label();
    emit_receiver_candidate(
        ops,
        relocations,
        view,
        plan,
        context_register,
        guard_miss,
        space_miss,
        candidate_ready,
    );
    dynasm!(ops ; .arch aarch64 ; =>candidate_ready);
    emit_receiver_publication(ops, view, context_register);
    dynasm!(ops ; .arch aarch64 ; b =>ready);
}

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
    emit_increment_runtime_counter(ops, context_register, RECEIVER_ALLOC_ATTEMPTS_OFFSET);

    let closure = ops.new_dynamic_label();
    let prototype_ready = ops.new_dynamic_label();
    emit_cell_test(ops, 2, 11, CellTest::IsNotCell, guard_miss);
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
    if !plan.class_allocation {
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
    emit_cell_test(ops, 14, 11, CellTest::IsNotCell, guard_miss);
    dynasm!(ops ; .arch aarch64
        ; ldrb w11, [x14] ; cmp w11, JS_CLOSURE_BODY_TYPE_TAG as u32 ; b.ne =>guard_miss
        ; ldr w11, [x14, view.closure_call_layout.function_id_byte]
    );
    emit_compare_function_id(ops, plan.new_target_function_id);
    dynasm!(ops ; .arch aarch64
        ; b.ne =>guard_miss ; =>target_ready
        ; ldr w4, [x2, view.class_constructor_layout.prototype_byte]
        ; cbz w4, =>guard_miss ; add x13, x12, x4 ; b =>prototype_ready
        ; =>closure
        ; ldr w11, [x2, view.closure_call_layout.function_id_byte]
    );
    emit_compare_function_id(ops, plan.new_target_function_id);
    dynasm!(ops ; .arch aarch64 ; b.ne =>guard_miss);
    // A nonzero sample proves the existing ledger owns this weak observation.
    // No generated operation creates a ledger entry or keeps it across GC.
    // The closure owns an internal ordinary property table, not an arbitrary
    // JS receiver. An empty sidecar retained by dictionary migration is legal;
    // matching shape and unmodified slot attributes remain authoritative.
    // The learned size, the bag and the prototype slot proof live in the
    // closure's rare record (`x17`); a closure without one was never
    // prepared as a constructor.
    dynasm!(ops ; .arch aarch64
        ; ldr w17, [x2, view.closure_call_layout.rare_byte]
        ; cbz w17, =>guard_miss ; add x17, x12, x17
        ; ldr w11, [x2, view.closure_call_layout.last_instance_byte]
        ; cbz w11, =>guard_miss ; add x13, x12, x11
        ; ldrh w11, [x13, view.object_slab_len_byte]
        ; ldrh w14, [x17, view.closure_call_layout.learned_instance_fields_byte]
        ; cmp w11, w14 ; csel w14, w11, w14, hi
        ; cmp w14, u32::from(plan.inline_capacity) ; b.hi =>guard_miss
        ; strh w14, [x17, view.closure_call_layout.learned_instance_fields_byte]
        ; ldr w11, [x17, view.closure_call_layout.own_props_byte]
        ; cbz w11, =>guard_miss ; add x13, x12, x11
        ; ldrb w14, [x13] ; cmp w14, OBJECT_BODY_TYPE_TAG ; b.ne =>guard_miss
    );
    crate::template::arm64::ic_probe::emit_ordinary_lookup_state_guard(ops, view, 13, guard_miss);
    let bag_inline = ops.new_dynamic_label();
    let bag_base = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; ldr w14, [x17, view.closure_call_layout.prototype_shape_byte] ; cbz w14, =>guard_miss
        ; ldr w15, [x13, view.object_shape_byte] ; cmp w14, w15 ; b.ne =>guard_miss
        ; ldr w14, [x17, view.closure_call_layout.prototype_slot_byte]
        ; ldrh w15, [x13, view.object_slab_len_byte] ; cmp w14, w15 ; b.hs =>guard_miss
        // The bag's slot base: in-object until it spills, then its slab's
        // words (`x12` holds the cage base).
        ; ldr w15, [x13, view.object_slab_handle_byte]
        ; cbz w15, =>bag_inline
        ; add x13, x12, x15
        ; add x13, x13, view.object_slab_words_byte
        ; b =>bag_base
        ; =>bag_inline
        ; add x13, x13, view.object_inline_values_byte
        ; =>bag_base
        ; ldr x4, [x13, w14, UXTW #3]
    );
    emit_cell_test(ops, 4, 11, CellTest::IsNotCell, guard_miss);
    dynasm!(ops ; .arch aarch64
        ; ldrb w14, [x4] ; cmp w14, OBJECT_BODY_TYPE_TAG ; b.ne =>guard_miss
        ; mov x13, x4 ; mov w4, w4
        ; =>prototype_ready
    );
    let prototype_count = usize::from(plan.prototype_shape_count);
    for (index, &shape) in plan
        .prototype_shapes
        .iter()
        .take(prototype_count)
        .enumerate()
    {
        dynasm!(ops
            ; .arch aarch64
            ; ldrb w14, [x13]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>guard_miss
            ; ldr w14, [x13, view.object_shape_byte]
        );
        emit_load_u64(ops, 15, u64::from(shape));
        dynasm!(ops
            ; .arch aarch64
            ; cmp w14, w15
            ; b.ne =>guard_miss
            ; ldr w14, [x13, view.jit_proto_byte]
        );
        if index + 1 != prototype_count {
            dynasm!(ops
                ; .arch aarch64
                ; cbz w14, =>guard_miss
                ; add x13, x12, x14
            );
        }
    }
    if prototype_count != 0 {
        dynasm!(ops ; .arch aarch64 ; cbnz w14, =>guard_miss);
    }

    // Only a complete cell fit below the buffer limit may mutate the nursery.
    // `x13` keeps the buffer for publication; the candidate lives at `top`.
    let cell_bytes = receiver_cell_bytes(view, plan);
    dynasm!(ops
        ; .arch aarch64
        ; ldr x13, [X(context_register), RECEIVER_ALLOC_LAB_OFFSET]
        ; ldr x16, [x13, LAB_TOP_OFFSET]
        ; ldr x14, [x13, LAB_LIMIT_OFFSET]
        ; add x15, x16, cell_bytes
        ; cmp x15, x14
        ; b.hi =>space_miss
    );

    // Initialize the header and the whole fixed body before publishing the
    // bump cursor. The body is four words: shape + null slab, prototype +
    // null sidecar, dictionary epoch + slot count + flags + in-object
    // capacity, and the unassigned dictionary id. In-object words past the
    // initial fields are never read before a store publishes them.
    let header_word = u64::from(OBJECT_BODY_TYPE_TAG)
        | (u64::from(otter_vm::jit::JIT_GC_YOUNG_FLAG) << 8)
        | (u64::from(cell_bytes) << 32);
    emit_load_u64(ops, 14, header_word);
    dynasm!(ops ; .arch aarch64 ; str x14, [x16]);
    emit_load_u64(ops, 14, u64::from(plan.receiver_shape));
    let layout_word = (u64::from(plan.initial_field_count) << 32)
        | (u64::from(otter_vm::jit::JIT_OBJECT_FLAG_EXTENSIBLE) << 48)
        | (u64::from(plan.inline_capacity) << 56);
    dynasm!(ops
        ; .arch aarch64
        ; str x14, [x16, view.object_shape_byte]
        ; str x4, [x16, view.jit_proto_byte]
    );
    emit_load_u64(ops, 14, layout_word);
    dynasm!(ops
        ; .arch aarch64
        ; str x14, [x16, view.object_dictionary_layout_byte]
        ; str xzr, [x16, view.object_dictionary_layout_byte + 8]
    );
    if plan.initial_field_count != 0 {
        emit_load_u64(ops, 14, VALUE_UNDEFINED);
        for index in 0..u32::from(plan.initial_field_count) {
            let offset = view.object_inline_values_byte + index * 8;
            dynasm!(ops ; .arch aarch64 ; str x14, [x16, offset]);
        }
    }
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, x16
        ; mov x1, x13
        ; b =>ready
    );
}

/// Cell bytes of the receiver `plan` allocates: the fixed object cell plus
/// its in-object slots.
fn receiver_cell_bytes(
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
) -> u32 {
    view.object_fixed_cell_bytes + 8 * u32::from(plan.inline_capacity)
}

/// Publish one completely initialized candidate from `x0` in buffer `x1`.
///
/// This operation cannot miss: the buffer bump was proven by
/// `emit_receiver_candidate`, the heap charged the buffer to any cap when it
/// was carved, and generated Machine code has no safepoint or reentry between
/// candidate creation and this effect. The cell size is read back from the
/// candidate's header, which the candidate wrote from its plan. Clobbers
/// `x13`–`x17`.
fn emit_receiver_publication(ops: &mut Assembler, view: &JitCompileSnapshot, context_register: u8) {
    dynasm!(ops
        ; .arch aarch64
        ; mov x16, x0
        ; mov x13, x1
        ; ldr w17, [x16, GC_HEADER_SIZE_BYTE]
        ; add x15, x16, x17
        ; str x15, [x13, LAB_TOP_OFFSET]
    );
    for (pointer_offset, by_size) in [
        (RECEIVER_ALLOC_TYPE_LIVE_BYTES_OFFSET, true),
        (RECEIVER_ALLOC_TYPE_COUNT_OFFSET, false),
        (RECEIVER_ALLOC_TYPE_BYTES_OFFSET, true),
    ] {
        dynasm!(ops
            ; .arch aarch64
            ; ldr x13, [X(context_register), pointer_offset]
            ; ldr x14, [x13]
        );
        if by_size {
            dynasm!(ops ; .arch aarch64 ; add x14, x14, x17);
        } else {
            dynasm!(ops ; .arch aarch64 ; add x14, x14, 1);
        }
        dynasm!(ops ; .arch aarch64 ; str x14, [x13]);
    }
    let observation_done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; ldrb w14, [x2] ; cmp w14, JS_CLOSURE_BODY_TYPE_TAG as u32 ; b.ne =>observation_done
        ; str w16, [x2, view.closure_call_layout.last_instance_byte]
        ; =>observation_done
    );
    emit_increment_runtime_counter(ops, context_register, RECEIVER_ALLOC_GENERATED_OFFSET);
    dynasm!(ops ; .arch aarch64 ; mov x0, x16);
}

/// Byte offset of the `u32` cell size inside a GC header.
const GC_HEADER_SIZE_BYTE: u32 = otter_vm::jit::JIT_GC_HEADER_SIZE_BYTES_OFFSET;

/// Complete the allocation half of a Machine receiver probe.
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
    emit_load_u64(ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch aarch64 ; mov x1, xzr);
    dynasm!(ops ; .arch aarch64 ; =>ready);
}

/// Commit one Machine receiver candidate after the probe's hit edge dominates.
pub(crate) fn emit_receiver_publication_effect(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    context_register: u8,
) {
    emit_receiver_publication(ops, view, context_register);
}
