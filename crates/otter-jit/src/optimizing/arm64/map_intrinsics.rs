//! Guarded compact-Map probes for the optimizing tier.
//!
//! # Contents
//! - Selection of `Map.get(Int32)` and existing-key `Map.set(Int32, value)`.
//! - Exact generated Fx/avalanche hashing and bounded collision-chain walks.
//! - In-place value overwrite with the ordinary old-to-young/marking barrier.
//!
//! # Invariants
//! - The caller has already proved the exact realm prototype method identity
//!   and leaves the guarded `MapBody` header in `x13`.
//! - Only exact stored Int32 hits complete here. Missing keys and SameValueZero
//!   cases requiring numeric canonicalization enter the canonical pre-effect
//!   method path.
//! - A chain walk is bounded; corruption or an adversarial long collision
//!   chain falls back before a `set` has any effect.
//! - Tables live in old space, so their header and entry addresses cannot move
//!   during the allocation-free probe or leaf write barrier.
//!
//! # See also
//! - `otter_vm::jit::JitMapTableLayout` — baked table and entry words.
//! - `crate::template::arm64::values::emit_write_barrier` — shared barrier.

use super::*;
use crate::template::arm64::values::{emit_box_int32, emit_load_symbol_u64};

const MAX_GENERATED_MAP_CHAIN: u32 = 64;

/// Whether one guarded collection method can complete from current SSA forms.
pub(super) fn guarded_map_intrinsic_is_supported(
    stub_id: otter_vm::native_abi::RuntimeStubId,
    reprs: &ReprMap,
    instruction: &SsaInstr,
    view: &JitCompileSnapshot,
) -> bool {
    let arguments = &instruction.inputs[1..];
    let key_is_int32 = arguments
        .first()
        .is_some_and(|key| reprs.representation(*key) == Representation::Int32);
    let layout = view.collection_layout.map_table;
    let layout_is_supported = layout.map_table_byte != 0
        && layout.table_buckets_byte != 0
        && layout.entry_size == 32
        && layout.entry_live_flag != 0;

    layout_is_supported
        && key_is_int32
        && ((stub_id == otter_vm::native_abi::STUB_COLLECTION_MAP_GET_LEAF.id
            && arguments.len() == 1)
            || (stub_id == otter_vm::native_abi::STUB_COLLECTION_MAP_SET_MUTATING.id
                && arguments.len() == 2))
}

/// Emit a selected compact-Map intrinsic, leaving its tagged result in `x0`.
pub(super) fn emit_guarded_map_intrinsic_body(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    stub_id: otter_vm::native_abi::RuntimeStubId,
    reprs: &ReprMap,
    allocation: &Allocation,
    instruction: &SsaInstr,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let is_get = stub_id == otter_vm::native_abi::STUB_COLLECTION_MAP_GET_LEAF.id;
    let is_set = stub_id == otter_vm::native_abi::STUB_COLLECTION_MAP_SET_MUTATING.id;
    if !is_get && !is_set {
        return Err(Unsupported::OperandShape("guarded Map intrinsic"));
    }

    let layout = view.collection_layout.map_table;
    let not_found = ops.new_dynamic_label();
    let loop_entry = ops.new_dynamic_label();
    let found = ops.new_dynamic_label();

    // Keep the exact boxed key in x8 for the collision check while x11 carries
    // its table hash. Map's Number hash is Fx([variant=3, f64 bits]) followed
    // by the same low-bit avalanche used by the VM.
    emit_load_location(ops, allocation.location(instruction.inputs[1]), 9)?;
    emit_box_int32(ops, 9, 12);
    dynasm!(ops ; .arch aarch64 ; mov x8, x9 ; scvtf d16, w9 ; fmov x11, d16);
    emit_load_u64(ops, 10, layout.fx_hash_multiplier);
    emit_load_u64(
        ops,
        12,
        layout
            .number_hash_tag
            .wrapping_mul(layout.fx_hash_multiplier),
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x11, x11, x12
        ; mul x11, x11, x10
        ; ror x11, x11, #38
        ; lsr x12, x11, #33
        ; eor x11, x11, x12
    );
    emit_load_u64(ops, 10, layout.hash_avalanche_1);
    dynasm!(ops
        ; .arch aarch64
        ; mul x11, x11, x10
        ; lsr x12, x11, #33
        ; eor x11, x11, x12
    );
    emit_load_u64(ops, 10, layout.hash_avalanche_2);
    dynasm!(ops
        ; .arch aarch64
        ; mul x11, x11, x10
        ; lsr x12, x11, #33
        ; eor x11, x11, x12
    );

    // x13 begins as the Map header and becomes the table header. The latter is
    // the actual parent of the overwritten value slot and is retained for the
    // generated write barrier.
    dynasm!(ops
        ; .arch aarch64
        ; ldr w12, [x13, layout.map_table_byte]
        ; cbz w12, =>not_found
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        10,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    let table_type_tag = u32::from(layout.table_type_tag);
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x10, x12
        ; ldrb w12, [x13]
        ; cmp w12, table_type_tag
        ; b.ne =>miss
        ; ldr w14, [x13, layout.table_len_byte]
        ; ldr w15, [x13, layout.table_bucket_mask_byte]
        ; and w11, w11, w15
        ; add x10, x13, layout.table_buckets_byte
        ; ldr w11, [x10, x11, lsl #2]
        ; add w15, w15, #1
        ; add x10, x10, x15, lsl #2
        ; add x10, x10, #7
        ; and x10, x10, #0xfffffffffffffff8
        ; cmn w11, #1
        ; b.eq =>not_found
        ; movz w15, MAX_GENERATED_MAP_CHAIN
        ; =>loop_entry
        ; cmp w11, w14
        ; b.hs =>miss
        ; add x9, x10, x11, lsl #5
        ; ldr w12, [x9, layout.entry_flags_byte]
        ; tst w12, layout.entry_live_flag
        ; b.eq =>miss
        ; ldr x12, [x9, layout.entry_key_byte]
        ; cmp x12, x8
        ; b.eq =>found
        ; ldr w11, [x9, layout.entry_next_byte]
        ; cmn w11, #1
        ; b.eq =>not_found
        ; subs w15, w15, #1
        ; b.eq =>miss
        ; b =>loop_entry
        ; =>found
    );

    if is_get {
        dynasm!(ops ; .arch aarch64 ; ldr x0, [x9, layout.entry_value_byte]);
    } else {
        let barrier_done = ops.new_dynamic_label();
        emit_load_boxed_value(ops, reprs, allocation, instruction.inputs[2], 10)?;
        dynasm!(ops ; .arch aarch64 ; str x10, [x9, layout.entry_value_byte]);
        emit_cell_test(ops, 10, 11, CellTest::IsNotCell, barrier_done);
        crate::template::arm64::values::emit_write_barrier(ops, relocations, view, 13, 10);
        dynasm!(ops ; .arch aarch64 ; =>barrier_done);
        emit_load_boxed_value(ops, reprs, allocation, instruction.inputs[0], 0)?;
    }
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>not_found);
    dynasm!(ops ; .arch aarch64 ; b =>miss);
    dynasm!(ops ; .arch aarch64 ; =>done);
    Ok(())
}
