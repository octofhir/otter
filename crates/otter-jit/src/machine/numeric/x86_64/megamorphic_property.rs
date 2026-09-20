//! Pure generated loads from the isolate's shared property lookup table.
//!
//! # Contents
//! - [`emit`] selects an existing table entry and reads its current data slot.
//!
//! # Invariants
//! - The receiver is consumed before scratch/output initialization.
//! - Every shape, ordinary-state, key, data-kind and bounds check precedes the
//!   field read. A miss returns undefined/false to the existing property CFG.
//! - The instruction cannot allocate, call or reenter. Table, shape, object and
//!   slab pointers are scratch only and never survive the instruction.
//! - Scratch is covered by the target's existing PropertyLoad clobber set.
//!
//! # See also
//! - `otter_vm::jit::JitPropertyLookupCache` is the one VM-owned table layout.
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
