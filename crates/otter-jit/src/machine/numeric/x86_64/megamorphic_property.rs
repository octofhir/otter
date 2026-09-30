//! Generated loads, own-data stores, and add transitions through shared tables.
//!
//! # Contents
//! - [`emit`] selects an existing table entry and reads its current data slot.
//! - [`emit_store`] writes an own data slot or appends a guarded transition.
//!
//! # Invariants
//! - The receiver is consumed before scratch/output initialization.
//! - Every shape, ordinary-state, key, data-kind and bounds check precedes the
//!   field access. A miss returns undefined/false to the existing property CFG.
//! - The instruction cannot allocate, call or reenter. Table, shape, object and
//!   slab pointers are scratch only and never survive the instruction.
//! - Scratch is covered by the target's existing PropertyLoad clobber set.
//!
//! # See also
//! - `otter_vm::jit::{JitPropertyLookupCache, JitStoreTransitionCache}`
//!   describe the VM-owned table layouts.
//! - `super::ordinary_lookup_state_guard` shares the named-property proof.

use super::*;

pub(super) fn emit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    locations: &[AllocatedLocation],
    atom: u32,
) -> Result<(), Unsupported> {
    let cache = view
        .property_lookup_cache
        .filter(|cache| cache.table_addr != 0 && view.cage_base != 0)
        .ok_or(Unsupported::OperandShape("x86-64 shared property table"))?;
    let [receiver, payload, hit] = locations else {
        return Err(Unsupported::OperandShape("x86-64 shared property operands"));
    };
    let miss = ops.new_dynamic_label();
    let holder = ops.new_dynamic_label();
    let spilled = ops.new_dynamic_label();
    let load = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();

    object_header(ops, relocations, view, frame, *receiver, miss)?;
    ordinary_lookup_state_guard(ops, view, miss);
    dynasm!(ops
        ; .arch x64
        ; mov eax, [r11 + view.object_shape_byte as i32]
    );
    symbolic(
        ops,
        relocations,
        10,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add rax, r10
        // A dictionary shape is shared by its lineage's dictionary objects
        // whatever their keys; no cache entry names one.
        ; test BYTE [rax + view.shape_kind_byte as i32], otter_vm::jit::JIT_SHAPE_KIND_DICTIONARY as i8
        ; jnz =>miss
        ; mov r9, [rax + cache.shape_id_byte as i32]
        ; mov rax, r9
    );
    load64(ops, 2, cache.hash_shape_multiplier);
    dynasm!(ops ; .arch x64 ; imul rax, rdx);
    load64(
        ops,
        2,
        u64::from(atom).wrapping_mul(cache.hash_atom_multiplier),
    );
    dynasm!(ops
        ; .arch x64
        ; xor rax, rdx
        ; shr rax, cache.hash_shift as i8
        ; and eax, cache.index_mask as i32
        ; imul rax, rax, cache.entry_bytes as i32
    );
    symbolic(
        ops,
        relocations,
        8,
        cache.table_addr as u64,
        RelocationTarget::PropertyLookupCacheTable,
    );
    dynasm!(ops
        ; .arch x64
        ; add r8, rax
        ; cmp QWORD [r8 + cache.receiver_shape_id_byte as i32], r9
        ; jne =>miss
        ; cmp DWORD [r8 + cache.atom_byte as i32], atom as i32
        ; jne =>miss
        ; cmp BYTE [r8 + cache.is_data_byte as i32], 1
        ; jne =>miss
        ; movzx eax, BYTE [r8 + cache.hops_byte as i32]
        ; cmp eax, 1
        ; ja =>miss
        ; test eax, eax
        ; jz =>holder
    );
    symbolic(
        ops,
        relocations,
        10,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; mov rax, [r8 + cache.validity_byte as i32]
        ; test rax, rax
        ; jz =>miss
        ; cmp DWORD [rax], 0
        ; je =>miss
        ; mov eax, [r8 + cache.holder_root_byte as i32]
        ; mov r11d, [r10 + rax + view.shape_prototype_byte as i32]
        ; add r11, r10
        ; =>holder
        // The matched holder shape names the slot, so the slot is live.
        ; movzx edx, WORD [r8 + cache.slot_byte as i32]
        ; mov r10d, [r11 + view.object_slab_handle_byte as i32]
        ; test r10d, r10d
        ; jnz =>spilled
        ; movzx eax, BYTE [r11 + view.object_inline_capacity_byte as i32]
        ; cmp edx, eax
        ; jae =>miss
        ; lea r8, [r11 + view.object_inline_values_byte as i32]
        ; jmp =>load
        ; =>spilled
    );
    symbolic(
        ops,
        relocations,
        9,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add r10, r9
        ; cmp edx, [r10 + view.object_slab_capacity_byte as i32]
        ; jae =>miss
        ; lea r8, [r10 + view.object_slab_words_byte as i32]
        ; =>load
        ; mov r8, [r8 + rdx * 8]
        ; mov r9d, 1
        ; jmp =>done
        ; =>miss
    );
    load64(ops, 8, VALUE_UNDEFINED);
    dynasm!(ops ; .arch x64 ; xor r9d, r9d ; =>done);
    store_integer(ops, frame, *payload, 8)?;
    store_integer(ops, frame, *hit, 9)?;
    Ok(())
}

pub(super) fn emit_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    locations: &[AllocatedLocation],
    atom: u32,
) -> Result<(), Unsupported> {
    let cache = view
        .property_lookup_cache
        .filter(|cache| cache.table_addr != 0 && view.cage_base != 0)
        .ok_or(Unsupported::OperandShape("x86-64 shared property table"))?;
    let [receiver, value, owner, hit] = locations else {
        return Err(Unsupported::OperandShape(
            "x86-64 shared property store operands",
        ));
    };
    let miss = ops.new_dynamic_label();
    let inline = ops.new_dynamic_label();
    let store = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let final_miss = ops.new_dynamic_label();

    object_header(ops, relocations, view, frame, *receiver, miss)?;
    ordinary_lookup_state_guard(ops, view, miss);
    dynasm!(ops ; .arch x64 ; test BYTE [r11 + view.object_flags_byte as i32], otter_vm::jit::JIT_OBJECT_FLAG_USED_AS_PROTOTYPE as i8 ; jnz =>miss);
    dynasm!(ops
        ; .arch x64
        ; mov eax, [r11 + view.object_shape_byte as i32]
    );
    symbolic(
        ops,
        relocations,
        10,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add rax, r10
        // A dictionary shape is shared by its lineage's dictionary objects
        // whatever their keys; no cache entry names one.
        ; test BYTE [rax + view.shape_kind_byte as i32], otter_vm::jit::JIT_SHAPE_KIND_DICTIONARY as i8
        ; jnz =>miss
        ; mov r9, [rax + cache.shape_id_byte as i32]
        ; mov rax, r9
    );
    load64(ops, 2, cache.hash_shape_multiplier);
    dynasm!(ops ; .arch x64 ; imul rax, rdx);
    load64(
        ops,
        2,
        u64::from(atom).wrapping_mul(cache.hash_atom_multiplier),
    );
    dynasm!(ops
        ; .arch x64
        ; xor rax, rdx
        ; shr rax, cache.hash_shift as i8
        ; and eax, cache.index_mask as i32
        ; imul rax, rax, cache.entry_bytes as i32
    );
    symbolic(
        ops,
        relocations,
        8,
        cache.table_addr as u64,
        RelocationTarget::PropertyLookupCacheTable,
    );
    dynasm!(ops
        ; .arch x64
        ; add r8, rax
        ; cmp QWORD [r8 + cache.receiver_shape_id_byte as i32], r9
        ; jne =>miss
        ; cmp DWORD [r8 + cache.atom_byte as i32], atom as i32
        ; jne =>miss
        ; cmp BYTE [r8 + cache.hops_byte as i32], 0
        ; jne =>miss
        ; cmp BYTE [r8 + cache.is_data_byte as i32], 1
        ; jne =>miss
        ; cmp BYTE [r8 + cache.is_writable_byte as i32], 1
        ; jne =>miss
        ; mov eax, [r11 + view.object_shape_byte as i32]
        ; cmp eax, [r8 + cache.holder_shape_byte as i32]
        ; jne =>miss
        // The matched holder shape names the slot, so the slot is live.
        ; movzx edx, WORD [r8 + cache.slot_byte as i32]
        ; mov r10d, [r11 + view.object_slab_handle_byte as i32]
        ; test r10d, r10d
        ; jz =>inline
    );
    symbolic(
        ops,
        relocations,
        9,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add r10, r9
        ; cmp edx, [r10 + view.object_slab_capacity_byte as i32]
        ; jae =>miss
        ; lea r8, [r10 + view.object_slab_words_byte as i32]
        ; jmp =>store
        ; =>inline
        ; movzx eax, BYTE [r11 + view.object_inline_capacity_byte as i32]
        ; cmp edx, eax
        ; jae =>miss
        ; lea r8, [r11 + view.object_inline_values_byte as i32]
        ; =>store
    );
    load_integer(ops, frame, *value, 10)?;
    dynasm!(ops
        ; .arch x64
        ; mov [r8 + rdx * 8], r10
        ; mov rax, r11
        ; mov r9d, 1
        ; jmp =>done
        ; =>miss
    );
    if let Some(transitions) = view.store_transition_cache {
        emit_transition_store(
            ops,
            relocations,
            view,
            frame,
            *receiver,
            *value,
            atom,
            transitions,
            final_miss,
            done,
        )?;
    }
    dynasm!(ops ; .arch x64 ; =>final_miss ; xor eax, eax ; xor r9d, r9d ; =>done);
    store_integer(ops, frame, *owner, 0)?;
    store_integer(ops, frame, *hit, 9)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_transition_store(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    receiver: AllocatedLocation,
    value: AllocatedLocation,
    atom: u32,
    cache: otter_vm::jit::JitStoreTransitionCache,
    miss: DynamicLabel,
    done: DynamicLabel,
) -> Result<(), Unsupported> {
    if cache.table_addr == 0 || cache.entry_bytes == 0 || cache.ways == 0 || cache.hash_shift >= 64
    {
        return Err(Unsupported::OperandShape("shared store transition table"));
    }
    let chain_done = ops.new_dynamic_label();
    let inline = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    object_header(ops, relocations, view, frame, receiver, miss)?;
    ordinary_lookup_state_guard(ops, view, miss);
    dynasm!(ops
        ; .arch x64
        ; mov rsi, r11
        ; mov eax, [r11 + view.object_shape_byte as i32]
    );
    symbolic(
        ops,
        relocations,
        10,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add rax, r10
        // A dictionary shape is shared by its lineage's dictionary objects
        // whatever their keys; no cache entry names one.
        ; test BYTE [rax + view.shape_kind_byte as i32], otter_vm::jit::JIT_SHAPE_KIND_DICTIONARY as i8
        ; jnz =>miss
        ; mov r9, [rax + cache.shape_id_byte as i32]
        ; xor edx, edx
    );
    dynasm!(ops ; .arch x64 ; mov rax, r9);
    load64(ops, 10, cache.hash_shape_multiplier);
    dynasm!(ops ; .arch x64 ; imul rax, r10);
    load64(
        ops,
        10,
        u64::from(atom).wrapping_mul(cache.hash_atom_multiplier),
    );
    dynasm!(ops
        ; .arch x64
        ; xor rax, r10
        ; shr rax, cache.hash_shift as i8
        ; and eax, cache.index_mask as i32
        ; imul rax, rax, (cache.entry_bytes * cache.ways) as i32
    );
    symbolic(
        ops,
        relocations,
        8,
        cache.table_addr as u64,
        RelocationTarget::StoreTransitionCacheTable,
    );
    // Probe the set's ways in recording order; `r8` ends on the match.
    let way = ops.new_dynamic_label();
    let next_way = ops.new_dynamic_label();
    let found = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; add r8, rax
        ; mov r10d, cache.ways as i32
        ; =>way
        ; cmp QWORD [r8 + cache.receiver_shape_byte as i32], r9
        ; jne =>next_way
        ; cmp DWORD [r8 + cache.atom_byte as i32], atom as i32
        ; je =>found
        ; =>next_way
        ; add r8, cache.entry_bytes as i32
        ; dec r10d
        ; jnz =>way
        ; jmp =>miss
        ; =>found
        ; cmp DWORD [r8 + cache.target_shape_byte as i32], 0
        ; je =>miss
        ; mov rax, [r8 + cache.validity_byte as i32]
        ; test rax, rax
        ; jz =>chain_done
        ; cmp DWORD [rax], 0
        ; je =>miss
        ; =>chain_done
        ; mov r11, rsi
        ; test BYTE [r11 + view.object_flags_byte as i32], otter_vm::jit::JIT_OBJECT_FLAG_USED_AS_PROTOTYPE as i8
        ; jnz =>miss
        ; test BYTE [r11 + view.object_flags_byte as i32], otter_vm::jit::JIT_OBJECT_FLAG_EXTENSIBLE as i8
        ; jz =>miss
        // The matched receiver shape has exactly `slot` slots: the append
        // index is its property count.
        ; movzx edx, WORD [r8 + cache.slot_byte as i32]
        ; mov r9d, [r8 + cache.target_shape_byte as i32]
        ; mov eax, [r11 + view.object_slab_handle_byte as i32]
        ; test eax, eax
        ; jz =>inline
    );
    symbolic(
        ops,
        relocations,
        10,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch x64
        ; add rax, r10
        ; cmp edx, [rax + view.object_slab_capacity_byte as i32]
        ; jae =>miss
        ; lea r8, [rax + view.object_slab_words_byte as i32]
        ; jmp =>ready
        ; =>inline
        ; movzx eax, BYTE [r11 + view.object_inline_capacity_byte as i32]
        ; cmp edx, eax
        ; jae =>miss
        ; lea r8, [r11 + view.object_inline_values_byte as i32]
        ; =>ready
    );
    load_integer(ops, frame, value, 10)?;
    dynasm!(ops
        ; .arch x64
        ; mov [r8 + rdx * 8], r10
        ; mov [r11 + view.object_shape_byte as i32], r9d
        ; mov rax, r11
        ; mov r9d, 1
        ; jmp =>done
    );
    Ok(())
}
