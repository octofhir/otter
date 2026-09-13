//! Shared nursery receiver allocation after live constructor/prototype proofs.
//!
//! # Contents
//! - Class-wrapper and ordinary-closure prototype resolution before effects.
//! - Existing nursery, accounting and complete object initialization program.
//!
//! # Invariants
//! - Ordinary closure probes require an active weak-observation ledger entry;
//!   GC flush invalidates that permission before moving either sampled object.
//! - Descriptor/shape guards load the live own prototype slot, never a cached
//!   prototype value. Every uncertain case reaches rooted canonical preparation.
//! - Header, slots and prototype are initialized before publishing the bump.
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
    dynasm!(ops ; .arch aarch64
        ; ldr w11, [x2, view.closure_call_layout.last_instance_byte]
        ; cbz w11, =>guard_miss ; add x13, x12, x11
        ; ldrh w11, [x13, view.object_slab_len_byte]
        ; ldrh w14, [x2, view.closure_call_layout.learned_instance_fields_byte]
        ; cmp w11, w14 ; csel w14, w11, w14, hi
        ; cmp w14, view.object_inline_slot_cap ; b.hi =>guard_miss
        ; strh w14, [x2, view.closure_call_layout.learned_instance_fields_byte]
        ; ldr w11, [x2, view.closure_call_layout.own_props_byte]
        ; cbz w11, =>guard_miss ; add x13, x12, x11
        ; ldrb w14, [x13] ; cmp w14, OBJECT_BODY_TYPE_TAG ; b.ne =>guard_miss
        ; ldrb w14, [x13, view.object_shape_cache_mode_byte]
        ; cmp w14, view.object_shape_cache_fast as u32 ; b.ne =>guard_miss
        ; ldrb w14, [x13, view.object_slot_attrs_overridden_byte] ; cbnz w14, =>guard_miss
        ; ldr w14, [x2, view.closure_call_layout.prototype_shape_byte] ; cbz w14, =>guard_miss
        ; ldr w15, [x13, view.object_shape_byte] ; cmp w14, w15 ; b.ne =>guard_miss
        ; ldr w14, [x2, view.closure_call_layout.prototype_slot_byte]
        ; ldrh w15, [x13, view.object_slab_len_byte] ; cmp w14, w15 ; b.hs =>guard_miss
        ; ldr x13, [x13, view.object_values_ptr_byte] ; cbz x13, =>guard_miss
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

    // Only a live NewFrom page and a complete fixed cell fit may mutate the
    // nursery. Collector handshakes deliberately publish a null/stale window.
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [X(context_register), THREAD_OFFSET]
        ; ldr x10, [x10, VM_THREAD_MARKING_FLAG_CELL_OFFSET]
        ; ldrb w10, [x10]
        ; cbnz w10, =>space_miss
        ; ldr x13, [X(context_register), RECEIVER_ALLOC_PAGE_OFFSET]
        ; cbz x13, =>space_miss
        ; ldrb w14, [x13, PAGE_SPACE_OFFSET]
        ; cmp w14, NEW_FROM_SPACE_KIND
        ; b.ne =>space_miss
        ; ldr x14, [x13, PAGE_BUMP_CURSOR_OFFSET]
        ; add x15, x14, view.object_cell_bytes
    );
    emit_load_u64(ops, 11, u64::from(GC_PAGE_SIZE));
    dynasm!(ops
        ; .arch aarch64
        ; cmp x15, x11
        ; b.hi =>space_miss
        ; add x16, x13, x14
        ; ldr x10, [X(context_register), RECEIVER_ALLOC_TRACKED_BYTES_OFFSET]
        ; cbz x10, >cap_ready
        ; ldr x9, [x10]
        ; add x9, x9, view.object_cell_bytes
        ; ldr x11, [X(context_register), RECEIVER_ALLOC_MAX_HEAP_BYTES_OFFSET]
        ; cmp x9, x11
        ; b.hi =>space_miss
        ; cap_ready:
    );

    // Initialize the complete fixed cell before publishing its bump cursor.
    for offset in (0..view.object_cell_bytes).step_by(16) {
        dynasm!(ops ; .arch aarch64 ; stp xzr, xzr, [x16, offset as i32]);
    }
    let header_word = u64::from(OBJECT_BODY_TYPE_TAG)
        | (u64::from(otter_vm::jit::JIT_GC_YOUNG_FLAG) << 8)
        | (u64::from(view.object_cell_bytes) << 32);
    emit_load_u64(ops, 14, header_word);
    dynasm!(ops ; .arch aarch64 ; str x14, [x16]);
    emit_load_u64(ops, 14, u64::from(plan.receiver_shape));
    dynasm!(ops
        ; .arch aarch64
        ; str w14, [x16, view.object_shape_byte]
        ; str w4, [x16, view.jit_proto_byte]
        ; mov w14, #1
        ; strb w14, [x16, view.object_extensible_byte]
    );
    if plan.initial_field_count != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; add x14, x16, view.object_inline_values_byte
            ; str x14, [x16, view.object_values_ptr_byte]
        );
    }
    emit_load_u64(ops, 14, VALUE_UNDEFINED);
    for index in 0..view.object_inline_slot_cap {
        let offset = view.object_inline_values_byte + index * 8;
        dynasm!(ops ; .arch aarch64 ; str x14, [x16, offset]);
    }
    dynasm!(ops
        ; .arch aarch64
        ; mov w14, plan.initial_field_count as u32
        ; strh w14, [x16, view.object_slab_len_byte]
        ; str x15, [x13, PAGE_BUMP_CURSOR_OFFSET]
        ; ldr x14, [x13, PAGE_ALLOCATED_BYTES_OFFSET]
        ; add x14, x14, view.object_cell_bytes
        ; str x14, [x13, PAGE_ALLOCATED_BYTES_OFFSET]
        ; cbz x10, >cap_committed
        ; str x9, [x10]
        ; cap_committed:
    );
    for (pointer_offset, increment) in [
        (
            RECEIVER_ALLOC_TYPE_LIVE_BYTES_OFFSET,
            view.object_cell_bytes,
        ),
        (RECEIVER_ALLOC_TYPE_COUNT_OFFSET, 1),
        (RECEIVER_ALLOC_TYPE_BYTES_OFFSET, view.object_cell_bytes),
    ] {
        dynasm!(ops
            ; .arch aarch64
            ; ldr x13, [X(context_register), pointer_offset]
            ; ldr x14, [x13]
            ; add x14, x14, increment
            ; str x14, [x13]
        );
    }
    let observation_done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; ldrb w14, [x2] ; cmp w14, JS_CLOSURE_BODY_TYPE_TAG as u32 ; b.ne =>observation_done
        ; str w16, [x2, view.closure_call_layout.last_instance_byte]
        ; =>observation_done
    );
    emit_increment_runtime_counter(ops, context_register, RECEIVER_ALLOC_GENERATED_OFFSET);
    dynasm!(ops ; .arch aarch64 ; mov x0, x16 ; b =>ready);
}
