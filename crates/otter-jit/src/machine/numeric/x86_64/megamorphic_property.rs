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
        ; test eax, eax
        ; jz =>miss
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
        ; mov r11d, [r11 + view.jit_proto_byte as i32]
        ; test r11d, r11d
        ; jz =>miss
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
        ; add r11, r10
        ; cmp BYTE [r11], OBJECT_BODY_TYPE_TAG as i8
        ; jne =>miss
    );
    ordinary_lookup_state_guard(ops, view, miss);
    dynasm!(ops
        ; .arch x64
        ; =>holder
        ; mov eax, [r11 + view.object_shape_byte as i32]
        ; test eax, eax
        ; jz =>miss
        ; cmp eax, [r8 + cache.holder_shape_byte as i32]
        ; jne =>miss
        ; movzx edx, WORD [r8 + cache.slot_byte as i32]
        ; movzx eax, WORD [r11 + view.object_slab_len_byte as i32]
        ; cmp edx, eax
        ; jae =>miss
        ; mov r10d, [r11 + view.object_slab_handle_byte as i32]
        ; test r10d, r10d
        ; jnz =>spilled
        ; cmp edx, view.object_inline_slot_cap as i32
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
        ; mov r8, [r11 + view.object_values_ptr_byte as i32]
        ; test r8, r8
        ; jz =>miss
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
    dynasm!(ops
        ; .arch x64
        ; mov eax, [r11 + view.object_shape_byte as i32]
        ; test eax, eax
        ; jz =>miss
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
        ; movzx edx, WORD [r8 + cache.slot_byte as i32]
        ; movzx eax, WORD [r11 + view.object_slab_len_byte as i32]
        ; cmp edx, eax
        ; jae =>miss
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
        ; mov r8, [r11 + view.object_values_ptr_byte as i32]
        ; test r8, r8
        ; jz =>miss
        ; jmp =>store
        ; =>inline
        ; cmp edx, view.object_inline_slot_cap as i32
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
    if cache.table_addr == 0 || cache.entry_bytes == 0 || cache.hash_shift >= 64 {
        return Err(Unsupported::OperandShape("shared store transition table"));
    }
    let proto_ready = ops.new_dynamic_label();
    let chain_loop = ops.new_dynamic_label();
    let chain_done = ops.new_dynamic_label();
    let inline = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    object_header(ops, relocations, view, frame, receiver, miss)?;
    ordinary_lookup_state_guard(ops, view, miss);
    dynasm!(ops
        ; .arch x64
        ; mov rsi, r11
        ; mov eax, [r11 + view.object_shape_byte as i32]
        ; test eax, eax
        ; jz =>miss
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
        ; mov r9, [rax + cache.shape_id_byte as i32]
        ; xor edx, edx
        ; mov eax, [r11 + view.jit_proto_byte as i32]
        ; test eax, eax
        ; jz =>proto_ready
        ; add rax, r10
        ; mov r11, rax
        ; cmp BYTE [r11], OBJECT_BODY_TYPE_TAG as i8
        ; jne =>miss
    );
    shape_state_guard(ops, view, miss);
    dynasm!(ops
        ; .arch x64
        ; mov eax, [r11 + view.object_shape_byte as i32]
        ; test eax, eax
        ; jz =>miss
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
        ; mov rdx, [rax + cache.shape_id_byte as i32]
        ; =>proto_ready
        ; mov rax, r9
    );
    load64(ops, 10, cache.hash_shape_multiplier);
    dynasm!(ops ; .arch x64 ; imul rax, r10);
    load64(ops, 10, cache.hash_atom_multiplier);
    dynasm!(ops ; .arch x64 ; imul r10, rdx ; rol r10, 17 ; xor rax, r10);
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
        ; imul rax, rax, cache.entry_bytes as i32
    );
    symbolic(
        ops,
        relocations,
        8,
        cache.table_addr as u64,
        RelocationTarget::StoreTransitionCacheTable,
    );
    dynasm!(ops
        ; .arch x64
        ; add r8, rax
        ; cmp QWORD [r8 + cache.receiver_shape_byte as i32], r9
        ; jne =>miss
        ; cmp QWORD [r8 + cache.prototype_shape_byte as i32], rdx
        ; jne =>miss
        ; cmp DWORD [r8 + cache.atom_byte as i32], atom as i32
        ; jne =>miss
        ; cmp DWORD [r8 + cache.target_shape_byte as i32], 0
        ; je =>miss
        ; movzx edx, BYTE [r8 + cache.chain_len_byte as i32]
        ; cmp edx, 8
        ; ja =>miss
        ; mov r11, rsi
        ; xor r9d, r9d
        ; test edx, edx
        ; jz =>chain_done
        ; =>chain_loop
        ; mov eax, [r11 + view.jit_proto_byte as i32]
        ; test eax, eax
        ; jz =>miss
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
        ; mov r11, rax
        ; cmp BYTE [r11], OBJECT_BODY_TYPE_TAG as i8
        ; jne =>miss
    );
    shape_state_guard(ops, view, miss);
    dynasm!(ops
        ; .arch x64
        ; mov eax, [r11 + view.object_shape_byte as i32]
        ; test eax, eax
        ; jz =>miss
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
        ; mov rax, [rax + cache.shape_id_byte as i32]
        ; cmp rax, [r8 + r9 * 8 + cache.chain_byte as i32]
        ; jne =>miss
        ; inc r9d
        ; cmp r9d, edx
        ; jb =>chain_loop
        ; =>chain_done
        ; cmp DWORD [r11 + view.jit_proto_byte as i32], 0
        ; jne =>miss
        ; mov r11, rsi
        ; cmp BYTE [r11 + view.object_extensible_byte as i32], 0
        ; je =>miss
        ; movzx edx, WORD [r8 + cache.slot_byte as i32]
        ; cmp dx, [r11 + view.object_slab_len_byte as i32]
        ; jne =>miss
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
        ; mov r8, [r11 + view.object_values_ptr_byte as i32]
        ; test r8, r8
        ; jz =>miss
        ; jmp =>ready
        ; =>inline
        ; cmp edx, view.object_inline_slot_cap as i32
        ; jae =>miss
        ; lea r8, [r11 + view.object_inline_values_byte as i32]
        ; =>ready
    );
    load_integer(ops, frame, value, 10)?;
    dynasm!(ops
        ; .arch x64
        ; mov [r8 + rdx * 8], r10
        ; mov [r11 + view.object_values_ptr_byte as i32], r8
        ; inc edx
        ; mov [r11 + view.object_slab_len_byte as i32], dx
        ; mov [r11 + view.object_shape_byte as i32], r9d
        ; mov rax, r11
        ; mov r9d, 1
        ; jmp =>done
    );
    Ok(())
}
