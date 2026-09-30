//! System V x86-64 nursery allocation for generated constructor receivers.
//!
//! # Contents
//! - Live `new.target` and prototype-chain guards.
//! - Fully initialized unpublished object construction at the heap's linear
//!   allocation buffer `top`.
//! - Separate candidate probing and atomic bump publication for Machine SSA.
//! - Allocator/statistics accounting shared by direct calls and SSA effects.
//!
//! # Invariants
//! - Every guard and capacity miss occurs before the buffer bump moves.
//! - The heap empties its buffer whenever marking, stress, tenuring or a heap
//!   cap needs the rooted path, so the bump is the only collector test.
//! - Header, the whole fixed body, and the initial in-object slots are
//!   initialized before publication makes the object visible to the collector.
//! - Closure prototype observations require the VM-owned live weak sample.
//!
//! # See also
//! - `crate::arm64::direct_call::receiver_allocation` — peer target emitter.
//! - `otter_vm::call_ops` — canonical rooted receiver preparation.

use super::*;
use crate::x86_64::allocation::emit_count_allocation;

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
    // The learned size and the `prototype` slot live in the closure's rare
    // record (`rcx`, free until the nursery reservation); a closure without
    // one was never prepared as a constructor.
    dynasm!(ops
        ; .arch x64
        ; mov ecx, [rdx + view.closure_call_layout.rare_byte as i32]
        ; test ecx, ecx
        ; jz =>guard_miss
        ; add rcx, r8
        ; mov r10d, [rdx + view.closure_call_layout.last_instance_byte as i32]
        ; test r10d, r10d
        ; jz =>guard_miss
        ; lea r9, [r8 + r10]
    );
    // The last receiver's slot count is its shape's; a dictionary shape
    // counts none, so a dictionary-mode receiver teaches nothing.
    dynasm!(ops
        ; .arch x64
        ; mov r10d, [r9 + view.object_shape_byte as i32]
        ; mov r10d, [r8 + r10 + view.shape_property_count_byte as i32]
        ; movzx r11d, WORD [rcx + view.closure_call_layout.learned_instance_fields_byte as i32]
        ; cmp r10d, r11d
        ; cmova r11d, r10d
        ; cmp r11d, i32::from(plan.inline_capacity)
        ; ja =>guard_miss
        ; mov [rcx + view.closure_call_layout.learned_instance_fields_byte as i32], r11w
        // The function's `prototype` slot: the hole until the default
        // object exists, which the cell test below rejects.
        ; mov rsi, [rcx + view.closure_call_layout.prototype_byte as i32]
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

    if let Some(validity) = plan.prototype_validity {
        symbolic(
            ops,
            relocations,
            11,
            validity.address as u64,
            RelocationTarget::PrototypeValidityCell {
                identity: validity.identity,
            },
        );
        dynasm!(ops ; .arch x64 ; cmp DWORD [r11], 0 ; je =>guard_miss);
        load64(ops, 11, u64::from(plan.prototype_root));
        dynasm!(ops ; .arch x64 ; cmp esi, [r8 + r11 + view.shape_prototype_byte as i32]
            ; jne =>guard_miss);
    }
    // The receiver's shape, into `esi` (the live prototype is no longer
    // needed once its lineage is proven).
    if plan.receiver_shape != 0 {
        load64(ops, 11, u64::from(plan.receiver_shape));
        dynasm!(ops
            ; .arch x64
            ; cmp esi, [r8 + r11 + view.shape_prototype_byte as i32]
            ; jne =>guard_miss
            ; mov esi, r11d
        );
    } else {
        dynasm!(ops
            ; .arch x64
            ; lea r9, [r8 + rsi]
            ; mov r10d, [r9 + view.object_exotic_handle_byte as i32]
            ; test r10d, r10d
            ; jz =>guard_miss
            ; mov esi, [r8 + r10 + view.exotic_instance_root_byte as i32]
            ; test esi, esi
            ; jz =>guard_miss
        );
    }

    // Only a complete cell fit below the buffer limit may mutate the nursery.
    // `rcx` keeps the buffer for publication; the candidate lives at `top`.
    let cell_bytes = receiver_cell_bytes(view, plan);
    dynasm!(ops
        ; .arch x64
        ; mov rcx, [r15 + ALLOC_WINDOW_LAB_OFFSET as i32]
        ; mov rax, [rcx + LAB_TOP_OFFSET as i32]
        ; lea r11, [rax + cell_bytes as i32]
        ; cmp r11, [rcx + LAB_LIMIT_OFFSET as i32]
        ; ja =>space_miss
    );

    // Initialize the header and the whole fixed body before publishing the
    // bump cursor. The header carries the flag and in-object capacity bytes;
    // the body is two words: shape + null slab, null sidecar + padding.
    // The receiver shape names exactly the initial fields, so the slot count
    // follows from it. In-object words past the initial fields are never
    // read before a store publishes them.
    load64(ops, 11, plan.cell_header_word(cell_bytes));
    dynasm!(ops
        ; .arch x64
        ; mov [rax], r11
        ; mov [rax + view.object_shape_byte as i32], rsi
        ; mov QWORD [rax + view.object_exotic_handle_byte as i32], 0
    );
    if plan.initial_field_count != 0 {
        load64(ops, 11, VALUE_UNDEFINED);
        for index in 0..u32::from(plan.initial_field_count) {
            let offset = view.object_inline_values_byte + index * 8;
            dynasm!(ops ; .arch x64 ; mov [rax + offset as i32], r11);
        }
    }
    dynasm!(ops ; .arch x64 ; jmp =>ready);
}

/// Cell bytes of the receiver `plan` allocates: the fixed object cell plus
/// its in-object slots.
fn receiver_cell_bytes(
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitReceiverAllocationPlan,
) -> u32 {
    view.object_fixed_cell_bytes + 8 * u32::from(plan.inline_capacity)
}

/// Publish one completely initialized candidate from `rax` in buffer `rcx`.
///
/// This operation cannot miss: the candidate probe proved the buffer bump
/// without a safepoint or reentry, and the heap charged the buffer to any cap
/// when it was carved. The cell size is read back from the candidate's
/// header, which the candidate wrote from its plan. Clobbers `r10` and `r11`.
fn emit_receiver_publication(ops: &mut Assembler, view: &JitCompileSnapshot) {
    let observation_done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r10d, [rax + otter_vm::jit::JIT_GC_HEADER_SIZE_BYTES_OFFSET as i32]
        ; lea r11, [rax + r10]
        ; mov [rcx + LAB_TOP_OFFSET as i32], r11
    );
    emit_count_allocation(ops, OBJECT_BODY_TYPE_TAG as u8, 10, 11);
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
/// unpublished receiver in `rax` and its allocation buffer in `rcx`; either pre-effect miss
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
