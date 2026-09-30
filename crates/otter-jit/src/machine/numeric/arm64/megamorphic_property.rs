//! AArch64 reads and writes through the isolate's shared property tables.
//!
//! # Contents
//! - An own or chain-proven inherited load producing a tagged value and hit bit.
//! - An own writable-data store producing an owner address and hit bit.
//! - A guarded add transition producing the same owner and hit bit.
//!
//! # Invariants
//! - Every key, state, holder and storage guard precedes a value read or write.
//! - No allocation, reentry, retained interior pointer or cached JS value exists.
//! - Miss returns undefined/false for the existing committed property sibling.
//! - Only the declared property clobbers and reserved x17 scratch are used.
//!
//! # See also
//! - `otter_vm::jit::{JitPropertyLookupCache, JitStoreTransitionCache}`
//!   describe the VM-owned table layouts.
//! - `super::super::property_cfg` owns cold completion and precise roots.

use super::*;

pub(super) fn emit(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    locations: &[AllocatedLocation],
    atom: u32,
) -> Result<(), Unsupported> {
    let cache = view.property_lookup_cache.ok_or(Unsupported::OperandShape(
        "megamorphic property table layout",
    ))?;
    if cache.table_addr == 0 || cache.entry_bytes == 0 || cache.hash_shift >= 64 {
        return Err(Unsupported::OperandShape(
            "megamorphic property table layout",
        ));
    }
    let miss = ops.new_dynamic_label();
    let holder = ops.new_dynamic_label();
    let inline = ops.new_dynamic_label();
    let load = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // Consume the allocator's late receiver before initializing any scratch.
    emit_load_header(
        ops,
        relocations,
        view,
        |ops, target| emit_load_allocated_tagged(ops, frame, locations[0], target, 0),
        13,
        miss,
    )?;
    emit_ordinary_lookup_state_guard(ops, view, 13, miss);
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; ldr w14, [x13, view.object_shape_byte]
        ; add x15, x16, x14
        // A dictionary shape is shared by its lineage's dictionary objects
        // whatever their keys; no cache entry names one.
        ; ldrb w12, [x15, view.shape_kind_byte]
        ; tbnz w12, crate::template::arm64::values::SHAPE_KIND_DICTIONARY_BIT, =>miss
        ; ldr x11, [x15, cache.shape_id_byte]
    );
    emit_load_u64(ops, 15, cache.hash_shape_multiplier);
    dynasm!(ops ; .arch aarch64 ; mul x12, x11, x15);
    emit_load_u64(
        ops,
        15,
        u64::from(atom).wrapping_mul(cache.hash_atom_multiplier),
    );
    dynasm!(ops ; .arch aarch64 ; eor x12, x12, x15 ; lsr x12, x12, u32::from(cache.hash_shift));
    emit_load_u64(ops, 15, u64::from(cache.index_mask));
    dynasm!(ops ; .arch aarch64 ; and x12, x12, x15);
    emit_load_u64(ops, 15, u64::from(cache.entry_bytes));
    dynasm!(ops ; .arch aarch64 ; mul x12, x12, x15);
    emit_load_symbolic_u64(
        ops,
        relocations,
        17,
        cache.table_addr as u64,
        RelocationTarget::PropertyLookupCacheTable,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x17, x17, x12
        ; ldr x12, [x17, cache.receiver_shape_id_byte]
        ; cmp x12, x11
        ; b.ne =>miss
        ; ldr w12, [x17, cache.atom_byte]
    );
    emit_load_u64(ops, 15, u64::from(atom));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w12, w15
        ; b.ne =>miss
        ; ldrb w12, [x17, cache.hops_byte]
        ; cmp w12, #1
        ; b.hi =>miss
        ; ldrb w15, [x17, cache.is_data_byte]
        ; cmp w15, #1
        ; b.ne =>miss
        ; cbz w12, =>holder
    );
    dynasm!(ops
        ; .arch aarch64
        ; ldr x12, [x17, cache.validity_byte]
        ; cbz x12, =>miss
        ; ldar w12, [x12]
        ; cbz w12, =>miss
        ; ldr w12, [x17, cache.holder_root_byte]
        ; add x12, x16, x12
        ; ldr w13, [x12, view.shape_prototype_byte]
        ; add x13, x16, x13
        ; =>holder
        // The matched holder shape names the slot, so the slot is live.
        ; ldrh w11, [x17, cache.slot_byte]
        ; ldr w12, [x13, view.object_slab_handle_byte]
        ; cbz w12, =>inline
        ; add x15, x16, x12
        ; ldr w12, [x15, view.object_slab_capacity_byte]
        ; cmp w11, w12
        ; b.hs =>miss
        ; add x15, x15, view.object_slab_words_byte
        ; b =>load
        ; =>inline
        ; ldrb w12, [x13, view.object_inline_capacity_byte]
        ; cmp w11, w12
        ; b.hs =>miss
        ; add x15, x13, view.object_inline_values_byte
        ; =>load
        ; lsl x11, x11, #3
        ; ldr x9, [x15, x11]
        ; mov x11, #1
        ; b =>done
        ; =>miss
    );
    emit_load_u64(ops, 9, VALUE_UNDEFINED);
    emit_load_u64(ops, 11, 0);
    dynasm!(ops ; .arch aarch64 ; =>done);
    emit_store_allocated_tagged(ops, frame, locations[1], 9, 0)?;
    emit_store_allocated_integer(ops, frame, locations[2], 11, 0)?;
    Ok(())
}

pub(super) fn emit_store(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    locations: &[AllocatedLocation],
    atom: u32,
) -> Result<(), Unsupported> {
    let cache = view.property_lookup_cache.ok_or(Unsupported::OperandShape(
        "megamorphic property table layout",
    ))?;
    let [receiver, value, owner, hit] = locations else {
        return Err(Unsupported::OperandShape(
            "megamorphic property store operands",
        ));
    };
    if cache.table_addr == 0 || cache.entry_bytes == 0 || cache.hash_shift >= 64 {
        return Err(Unsupported::OperandShape(
            "megamorphic property table layout",
        ));
    }
    let miss = ops.new_dynamic_label();
    let inline = ops.new_dynamic_label();
    let store = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let final_miss = ops.new_dynamic_label();
    emit_load_header(
        ops,
        relocations,
        view,
        |ops, target| emit_load_allocated_tagged(ops, frame, *receiver, target, 0),
        13,
        miss,
    )?;
    emit_ordinary_lookup_state_guard(ops, view, 13, miss);
    dynasm!(ops ; .arch aarch64 ; ldrb w14, [x13, view.object_flags_byte] ; tbnz w14, 4, =>miss);
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; ldr w14, [x13, view.object_shape_byte]
        ; add x15, x16, x14
        // A dictionary shape is shared by its lineage's dictionary objects
        // whatever their keys; no cache entry names one.
        ; ldrb w12, [x15, view.shape_kind_byte]
        ; tbnz w12, crate::template::arm64::values::SHAPE_KIND_DICTIONARY_BIT, =>miss
        ; ldr x11, [x15, cache.shape_id_byte]
    );
    emit_load_u64(ops, 15, cache.hash_shape_multiplier);
    dynasm!(ops ; .arch aarch64 ; mul x12, x11, x15);
    emit_load_u64(
        ops,
        15,
        u64::from(atom).wrapping_mul(cache.hash_atom_multiplier),
    );
    dynasm!(ops ; .arch aarch64 ; eor x12, x12, x15 ; lsr x12, x12, u32::from(cache.hash_shift));
    emit_load_u64(ops, 15, u64::from(cache.index_mask));
    dynasm!(ops ; .arch aarch64 ; and x12, x12, x15);
    emit_load_u64(ops, 15, u64::from(cache.entry_bytes));
    dynasm!(ops ; .arch aarch64 ; mul x12, x12, x15);
    emit_load_symbolic_u64(
        ops,
        relocations,
        17,
        cache.table_addr as u64,
        RelocationTarget::PropertyLookupCacheTable,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x17, x17, x12
        ; ldr x12, [x17, cache.receiver_shape_id_byte]
        ; cmp x12, x11
        ; b.ne =>miss
        ; ldr w12, [x17, cache.atom_byte]
    );
    emit_load_u64(ops, 15, u64::from(atom));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w12, w15
        ; b.ne =>miss
        ; ldrb w12, [x17, cache.hops_byte]
        ; cbnz w12, =>miss
        ; ldrb w12, [x17, cache.is_data_byte]
        ; cmp w12, #1
        ; b.ne =>miss
        ; ldrb w12, [x17, cache.is_writable_byte]
        ; cmp w12, #1
        ; b.ne =>miss
        ; ldr w12, [x17, cache.holder_shape_byte]
        ; cbz w12, =>miss
        ; cmp w14, w12
        ; b.ne =>miss
        // The matched holder shape names the slot, so the slot is live.
        ; ldrh w11, [x17, cache.slot_byte]
        ; ldr w12, [x13, view.object_slab_handle_byte]
        ; cbz w12, =>inline
        ; add x15, x16, x12
        ; ldr w12, [x15, view.object_slab_capacity_byte]
        ; cmp w11, w12
        ; b.hs =>miss
        ; add x15, x15, view.object_slab_words_byte
        ; b =>store
        ; =>inline
        ; ldrb w12, [x13, view.object_inline_capacity_byte]
        ; cmp w11, w12
        ; b.hs =>miss
        ; add x15, x13, view.object_inline_values_byte
        ; =>store
        ; lsl x11, x11, #3
    );
    emit_load_allocated_tagged(ops, frame, *value, 9, 0)?;
    dynasm!(ops
        ; .arch aarch64
        ; str x9, [x15, x11]
        ; mov x12, x13
        ; mov x11, #1
        ; b =>done
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
    dynasm!(ops ; .arch aarch64 ; =>final_miss);
    emit_load_u64(ops, 12, 0);
    emit_load_u64(ops, 11, 0);
    dynasm!(ops ; .arch aarch64 ; =>done);
    emit_store_allocated_integer(ops, frame, *owner, 12, 0)?;
    emit_store_allocated_integer(ops, frame, *hit, 11, 0)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_transition_store(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    frame: MachineFrameLayout,
    receiver: AllocatedLocation,
    value: AllocatedLocation,
    atom: u32,
    cache: otter_vm::jit::JitStoreTransitionCache,
    miss: dynasmrt::DynamicLabel,
    done: dynasmrt::DynamicLabel,
) -> Result<(), Unsupported> {
    if cache.table_addr == 0
        || cache.entry_bytes == 0
        || cache.entry_bytes >= 4096
        || cache.ways == 0
        || cache.ways > u32::from(u16::MAX)
        || cache.hash_shift >= 64
    {
        return Err(Unsupported::OperandShape("shared store transition table"));
    }
    let chain_done = ops.new_dynamic_label();
    let inline = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    emit_load_header(
        ops,
        relocations,
        view,
        |ops, target| emit_load_allocated_tagged(ops, frame, receiver, target, 0),
        13,
        miss,
    )?;
    emit_ordinary_lookup_state_guard(ops, view, 13, miss);
    emit_load_symbolic_u64(
        ops,
        relocations,
        16,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; mov x12, x13
        ; ldr w14, [x13, view.object_shape_byte]
        ; add x14, x16, x14
        ; ldrb w15, [x14, view.shape_kind_byte]
        ; tbnz w15, crate::template::arm64::values::SHAPE_KIND_DICTIONARY_BIT, =>miss
        ; ldr x11, [x14, cache.shape_id_byte]
        ; mov x15, xzr
    );
    emit_load_u64(ops, 10, cache.hash_shape_multiplier);
    dynasm!(ops ; .arch aarch64 ; mul x9, x11, x10);
    emit_load_u64(
        ops,
        10,
        u64::from(atom).wrapping_mul(cache.hash_atom_multiplier),
    );
    dynasm!(ops ; .arch aarch64 ; eor x9, x9, x10 ; lsr x9, x9, u32::from(cache.hash_shift));
    emit_load_u64(ops, 10, u64::from(cache.index_mask));
    dynasm!(ops ; .arch aarch64 ; and x9, x9, x10);
    emit_load_u64(ops, 10, u64::from(cache.entry_bytes * cache.ways));
    dynasm!(ops ; .arch aarch64 ; mul x9, x9, x10);
    emit_load_symbolic_u64(
        ops,
        relocations,
        17,
        cache.table_addr as u64,
        RelocationTarget::StoreTransitionCacheTable,
    );
    emit_load_u64(ops, 14, u64::from(atom));
    // Probe the set's ways in recording order; `x17` ends on the match.
    let way = ops.new_dynamic_label();
    let next_way = ops.new_dynamic_label();
    let found = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; add x17, x17, x9
        ; movz w9, cache.ways
        ; =>way
        ; ldr x10, [x17, cache.receiver_shape_byte]
        ; cmp x10, x11
        ; b.ne =>next_way
        ; ldr w10, [x17, cache.atom_byte]
        ; cmp w10, w14
        ; b.eq =>found
        ; =>next_way
        ; add x17, x17, cache.entry_bytes
        ; subs w9, w9, #1
        ; b.ne =>way
        ; b =>miss
        ; =>found
        ; ldr w10, [x17, cache.target_shape_byte]
        ; cbz w10, =>miss
        ; ldr x9, [x17, cache.validity_byte]
        ; cbz x9, =>chain_done
        ; ldar w9, [x9]
        ; cbz w9, =>miss
        ; =>chain_done
        ; ldrb w10, [x12, view.object_flags_byte]
        ; tbnz w10, 4, =>miss
        ; tbz w10, crate::template::arm64::ic_probe::EXTENSIBLE_BIT, =>miss
        // The matched receiver shape has exactly `slot` slots: the append
        // index is its property count.
        ; ldrh w9, [x17, cache.slot_byte]
        ; ldr w10, [x12, view.object_slab_handle_byte]
        ; cbz w10, =>inline
        ; add x15, x16, x10
        ; ldr w10, [x15, view.object_slab_capacity_byte]
        ; cmp w9, w10
        ; b.hs =>miss
        ; add x15, x15, view.object_slab_words_byte
        ; b =>ready
        ; =>inline
        ; ldrb w10, [x12, view.object_inline_capacity_byte]
        ; cmp w9, w10
        ; b.hs =>miss
        ; add x15, x12, view.object_inline_values_byte
        ; =>ready
    );
    emit_load_allocated_tagged(ops, frame, value, 10, 0)?;
    dynasm!(ops
        ; .arch aarch64
        ; str x10, [x15, x9, lsl #3]
        ; ldr w10, [x17, cache.target_shape_byte]
        ; str w10, [x12, view.object_shape_byte]
        ; mov x11, #1
        ; b =>done
    );
    Ok(())
}
