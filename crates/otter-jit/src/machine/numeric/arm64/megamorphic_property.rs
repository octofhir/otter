//! AArch64 reads through the isolate's existing shape/atom lookup table.
//!
//! # Contents
//! - A bounded own/direct-prototype probe producing a tagged value and hit bit.
//!
//! # Invariants
//! - Every key, state, holder and storage guard precedes the single value read.
//! - No allocation, reentry, retained interior pointer or cached JS value exists.
//! - Miss returns undefined/false for the existing committed property sibling.
//! - Only the declared PropertyLoad clobbers and reserved x17 scratch are used.
//!
//! # See also
//! - `otter_vm::jit::JitPropertyLookupCache` describes the one runtime table.
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
        ; cbz w14, =>miss
        ; add x15, x16, x14
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
        ; ldr w12, [x13, view.jit_proto_byte]
        ; cbz w12, =>miss
        ; add x13, x16, x12
        ; ldrb w14, [x13]
        ; cmp w14, u32::from(otter_vm::object::OBJECT_BODY_TYPE_TAG)
        ; b.ne =>miss
    );
    emit_ordinary_lookup_state_guard(ops, view, 13, miss);
    dynasm!(ops
        ; .arch aarch64
        ; =>holder
        ; ldr w14, [x13, view.object_shape_byte]
        ; ldr w12, [x17, cache.holder_shape_byte]
        ; cbz w12, =>miss
        ; cmp w14, w12
        ; b.ne =>miss
        ; ldrh w11, [x17, cache.slot_byte]
        ; ldrh w12, [x13, view.object_slab_len_byte]
        ; cmp w11, w12
        ; b.hs =>miss
        ; ldr w12, [x13, view.object_slab_handle_byte]
        ; cbz w12, =>inline
        ; add x15, x16, x12
        ; ldr w12, [x15, view.object_slab_capacity_byte]
        ; cmp w11, w12
        ; b.hs =>miss
        ; ldr x15, [x13, view.object_values_ptr_byte]
        ; cbz x15, =>miss
        ; b =>load
        ; =>inline
        ; cmp w11, view.object_inline_slot_cap
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
