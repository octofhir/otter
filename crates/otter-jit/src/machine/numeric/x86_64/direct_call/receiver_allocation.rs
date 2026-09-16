//! System V x86-64 nursery allocation for generated constructor receivers.
//!
//! # Contents
//! - Live `new.target` and prototype-chain guards.
//! - Fully initialized unpublished object construction in the active nursery.
//! - Separate candidate probing and atomic bump publication for Machine SSA.
//! - Allocator/statistics accounting shared by direct calls and SSA effects.
//!
//! # Invariants
//! - Every guard and capacity miss occurs before the nursery bump moves.
//! - Header, shape, prototype, slots, and values pointer are initialized before
//!   publication makes the object visible to the collector.
//! - Closure prototype observations require the VM-owned live weak sample.
//!
//! # See also
//! - `crate::arm64::direct_call::receiver_allocation` — peer target emitter.
//! - `otter_vm::call_ops` — canonical rooted receiver preparation.

use super::*;

pub(crate) fn emit_increment_runtime_counter(ops: &mut Assembler, offset: u32) {
    dynasm!(ops
        ; .arch x64
        ; mov r11, [r15 + RUNTIME_STATS_OFFSET as i32]
        ; add QWORD [r11 + offset as i32], 1
    );
}

/// Emit the complete no-safepoint receiver allocation program.
///
/// `rdx` contains the live `new.target`. A hit returns the tagged receiver in
/// `rax`; guard and nursery misses branch without performing an effect.
pub(crate) fn emit_generated_receiver_allocation(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
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
        guard_miss,
        space_miss,
        candidate_ready,
    );
    dynasm!(ops ; .arch x64 ; =>candidate_ready);
    emit_receiver_publication(ops, view);
    dynasm!(ops ; .arch x64 ; jmp =>ready);
}

#[allow(clippy::too_many_arguments)]
fn emit_receiver_candidate(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
    guard_miss: DynamicLabel,
    space_miss: DynamicLabel,
    ready: DynamicLabel,
) {
    emit_increment_runtime_counter(ops, RECEIVER_ALLOC_ATTEMPTS_OFFSET);

    let closure = ops.new_dynamic_label();
    let prototype_ready = ops.new_dynamic_label();
    let target_ready = ops.new_dynamic_label();
    let capacity_ready = ops.new_dynamic_label();

    // A JS cell is an aligned cage address with none of the immediate tag bits.
    dynasm!(ops ; .arch x64 ; test rdx, rdx ; jz =>guard_miss);
    load64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov r10, rdx
        ; and r10, r11
        ; test r10, r10
        ; jnz =>guard_miss
    );
    symbolic(
        ops,
        relocations,
        8,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; movzx r10d, BYTE [rdx]
        ; cmp r10d, JS_CLOSURE_BODY_TYPE_TAG as i32
        ; je =>closure
    );
    if !plan.class_allocation {
        dynasm!(ops ; .arch x64 ; jmp =>guard_miss);
    }
    dynasm!(ops
        ; .arch x64
        ; cmp r10d, view.class_constructor_layout.type_tag as i32
        ; jne =>guard_miss
        ; mov r11, [rdx + view.class_constructor_layout.callable_byte as i32]
    );
    load64(
        ops,
        10,
        otter_vm::value::tag::box_function_id(plan.new_target_function_id),
    );
    dynasm!(ops
        ; .arch x64
        ; cmp r11, r10
        ; je =>target_ready
        ; test r11, r11
        ; jz =>guard_miss
    );
    load64(ops, 10, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov r9, r11
        ; and r9, r10
        ; test r9, r9
        ; jnz =>guard_miss
        ; cmp BYTE [r11], JS_CLOSURE_BODY_TYPE_TAG as i8
        ; jne =>guard_miss
        ; cmp DWORD [r11 + view.closure_call_layout.function_id_byte as i32], plan.new_target_function_id as i32
        ; jne =>guard_miss
        ; =>target_ready
        ; mov esi, [rdx + view.class_constructor_layout.prototype_byte as i32]
        ; test esi, esi
        ; jz =>guard_miss
        ; lea r9, [r8 + rsi]
        ; jmp =>prototype_ready
        ; =>closure
        ; cmp DWORD [rdx + view.closure_call_layout.function_id_byte as i32], plan.new_target_function_id as i32
        ; jne =>guard_miss
    );
    dynasm!(ops
        ; .arch x64
        ; mov r10d, [rdx + view.closure_call_layout.last_instance_byte as i32]
        ; test r10d, r10d
        ; jz =>guard_miss
        ; lea r9, [r8 + r10]
    );
    dynasm!(ops
        ; .arch x64
        ; movzx r10d, WORD [r9 + view.object_slab_len_byte as i32]
        ; movzx r11d, WORD [rdx + view.closure_call_layout.learned_instance_fields_byte as i32]
        ; cmp r10d, r11d
        ; cmova r11d, r10d
        ; cmp r11d, view.object_inline_slot_cap as i32
        ; ja =>guard_miss
        ; mov [rdx + view.closure_call_layout.learned_instance_fields_byte as i32], r11w
        ; mov r10d, [rdx + view.closure_call_layout.own_props_byte as i32]
        ; test r10d, r10d
        ; jz =>guard_miss
        ; lea r9, [r8 + r10]
        ; cmp BYTE [r9], OBJECT_BODY_TYPE_TAG as i8
        ; jne =>guard_miss
        ; cmp BYTE [r9 + view.object_shape_cache_mode_byte as i32], view.object_shape_cache_fast as i8
        ; jne =>guard_miss
        ; cmp BYTE [r9 + view.object_slot_attrs_overridden_byte as i32], 0
        ; jne =>guard_miss
    );
    dynasm!(ops
        ; .arch x64
        ; mov r10d, [rdx + view.closure_call_layout.prototype_shape_byte as i32]
        ; test r10d, r10d
        ; jz =>guard_miss
    );
    dynasm!(ops
        ; .arch x64
        ; cmp r10d, [r9 + view.object_shape_byte as i32]
        ; jne =>guard_miss
    );
    dynasm!(ops
        ; .arch x64
        ; mov r10d, [rdx + view.closure_call_layout.prototype_slot_byte as i32]
        ; movzx r11d, WORD [r9 + view.object_slab_len_byte as i32]
        ; cmp r10d, r11d
        ; jae =>guard_miss
    );
    dynasm!(ops
        ; .arch x64
        ; mov r9, [r9 + view.object_values_ptr_byte as i32]
        ; test r9, r9
        ; jz =>guard_miss
    );
    dynasm!(ops
        ; .arch x64
        ; mov rsi, [r9 + r10 * 8]
        ; test rsi, rsi
        ; jz =>guard_miss
    );
    load64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov r10, rsi
        ; and r10, r11
        ; test r10, r10
        ; jnz =>guard_miss
        ; cmp BYTE [rsi], OBJECT_BODY_TYPE_TAG as i8
        ; jne =>guard_miss
        ; mov r9, rsi
        ; mov esi, esi
        ; =>prototype_ready
    );

    for (index, &shape) in plan
        .prototype_shapes
        .iter()
        .take(usize::from(plan.prototype_shape_count))
        .enumerate()
    {
        dynasm!(ops
            ; .arch x64
            ; cmp BYTE [r9], OBJECT_BODY_TYPE_TAG as i8
            ; jne =>guard_miss
            ; cmp DWORD [r9 + view.object_shape_byte as i32], shape as i32
            ; jne =>guard_miss
            ; mov r10d, [r9 + view.jit_proto_byte as i32]
        );
        if index + 1 != usize::from(plan.prototype_shape_count) {
            dynasm!(ops
                ; .arch x64
                ; test r10d, r10d
                ; jz =>guard_miss
                ; lea r9, [r8 + r10]
            );
        }
    }
    if plan.prototype_shape_count != 0 {
        dynasm!(ops ; .arch x64 ; test r10d, r10d ; jnz =>guard_miss);
    }

    // The collector withdraws this window during marking or refill. All
    // capacity and heap-limit checks precede writes and bump publication.
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + THREAD_OFFSET as i32]
        ; mov r10, [r10 + VM_THREAD_MARKING_FLAG_CELL_OFFSET as i32]
        ; cmp BYTE [r10], 0
        ; jne =>space_miss
        ; mov rcx, [r15 + RECEIVER_ALLOC_PAGE_OFFSET as i32]
        ; test rcx, rcx
        ; jz =>space_miss
        ; cmp BYTE [rcx + PAGE_SPACE_OFFSET as i32], NEW_FROM_SPACE_KIND as i8
        ; jne =>space_miss
        ; mov r10, [rcx + PAGE_BUMP_CURSOR_OFFSET as i32]
        ; lea r11, [r10 + view.object_cell_bytes as i32]
        ; cmp r11, GC_PAGE_SIZE as i32
        ; ja =>space_miss
        ; lea rax, [rcx + r10]
        ; mov r10, [r15 + RECEIVER_ALLOC_TRACKED_BYTES_OFFSET as i32]
        ; test r10, r10
        ; jz =>capacity_ready
        ; mov r9, [r10]
        ; add r9, view.object_cell_bytes as i32
        ; cmp r9, [r15 + RECEIVER_ALLOC_MAX_HEAP_BYTES_OFFSET as i32]
        ; ja =>space_miss
        ; =>capacity_ready
    );

    for offset in (0..view.object_cell_bytes).step_by(8) {
        dynasm!(ops ; .arch x64 ; mov QWORD [rax + offset as i32], 0);
    }
    let header_word = u64::from(OBJECT_BODY_TYPE_TAG)
        | (u64::from(otter_vm::jit::JIT_GC_YOUNG_FLAG) << 8)
        | (u64::from(view.object_cell_bytes) << 32);
    load64(ops, 11, header_word);
    dynasm!(ops
        ; .arch x64
        ; mov [rax], r11
        ; mov DWORD [rax + view.object_shape_byte as i32], plan.receiver_shape as i32
        ; mov [rax + view.jit_proto_byte as i32], esi
        ; mov BYTE [rax + view.object_extensible_byte as i32], 1
    );
    if plan.initial_field_count != 0 {
        dynasm!(ops
            ; .arch x64
            ; lea r11, [rax + view.object_inline_values_byte as i32]
            ; mov [rax + view.object_values_ptr_byte as i32], r11
        );
    }
    load64(ops, 11, VALUE_UNDEFINED);
    for index in 0..view.object_inline_slot_cap {
        let offset = view.object_inline_values_byte + index * 8;
        dynasm!(ops ; .arch x64 ; mov [rax + offset as i32], r11);
    }
    dynasm!(ops
        ; .arch x64
        ; mov WORD [rax + view.object_slab_len_byte as i32], plan.initial_field_count as i16
        ; jmp =>ready
    );
}

/// Publish one completely initialized candidate from `rax` on page `rcx`.
///
/// This operation cannot miss: the candidate probe has completed every
/// collector, capacity, and heap-limit proof without a safepoint or reentry.
fn emit_receiver_publication(ops: &mut Assembler, view: &JitCompileSnapshot) {
    let observation_done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r10, rax
        ; sub r10, rcx
        ; add r10, view.object_cell_bytes as i32
        ; mov [rcx + PAGE_BUMP_CURSOR_OFFSET as i32], r10
        ; add QWORD [rcx + PAGE_ALLOCATED_BYTES_OFFSET as i32], view.object_cell_bytes as i32
        ; mov r11, [r15 + RECEIVER_ALLOC_TRACKED_BYTES_OFFSET as i32]
        ; test r11, r11
        ; jz >tracked_done
        ; add QWORD [r11], view.object_cell_bytes as i32
        ; tracked_done:
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
            ; .arch x64
            ; mov r11, [r15 + pointer_offset as i32]
            ; add QWORD [r11], increment as i32
        );
    }
    dynasm!(ops
        ; .arch x64
        ; cmp BYTE [rdx], JS_CLOSURE_BODY_TYPE_TAG as i8
        ; jne =>observation_done
        ; mov [rdx + view.closure_call_layout.last_instance_byte as i32], eax
        ; =>observation_done
    );
    emit_increment_runtime_counter(ops, RECEIVER_ALLOC_GENERATED_OFFSET);
}

/// Complete the allocation half of a Machine receiver probe.
///
/// `rdx` is the live new.target input. A hit returns the initialized,
/// unpublished receiver in `rax` and its page in `rcx`; either pre-effect miss
/// returns undefined in `rax` and zero in `rcx`.
pub(in crate::machine::numeric) fn emit_receiver_candidate_probe(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
) {
    let guard_miss = ops.new_dynamic_label();
    let space_miss = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    emit_receiver_candidate(ops, relocations, view, plan, guard_miss, space_miss, ready);
    dynasm!(ops ; .arch x64 ; =>guard_miss);
    emit_increment_runtime_counter(ops, RECEIVER_ALLOC_GUARD_MISSES_OFFSET);
    dynasm!(ops ; .arch x64 ; jmp =>miss ; =>space_miss);
    emit_increment_runtime_counter(ops, RECEIVER_ALLOC_SPACE_MISSES_OFFSET);
    dynasm!(ops ; .arch x64 ; =>miss);
    load64(ops, 0, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; xor ecx, ecx ; =>ready);
}

/// Commit one Machine receiver candidate after the probe's hit edge dominates.
pub(in crate::machine::numeric) fn emit_receiver_publication_effect(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
) {
    emit_receiver_publication(ops, view);
}
