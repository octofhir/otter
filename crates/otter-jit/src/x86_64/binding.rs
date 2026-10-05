//! Live global binding geometry shared by x86-64 native tiers.
//!
//! # Contents
//! - Source-identified permanent lexical-cell address and live TDZ guards.
//! - Current global epoch, ordinary shape and watched dictionary guards.
//! - Shape-selected persistent-prefix/suffix banks and eligible global reads.
//!
//! # Invariants
//! - The VM proof and opcode schema own realm/name/descriptor eligibility.
//!   Every hit also proves the live active realm against the source snapshot.
//! - Helpers clobber only their two assigned GPs, r10/r11 and flags; reads
//!   commit their result after every pre-effect guard has succeeded.
//! - A lexical relocation identifies its actual source FID and byte PC. Its
//!   address is permanent and traced; its current value is never baked.
//! - Objects and suffix handles are reread on every access. No derived address
//!   survives a call, collection, eager exit or committed Template miss.
//! - Existence and assignment policies remain in the Template schema owner.
//!
//! # See also
//! - `otter_vm::jit::BindingHitProof` owns admitted scalar proof facts.
//! - [`super::fields`] owns the VM's shape-selected field address geometry.
//! - `crate::template::x86_64::binding` and `crate::graph::x86_64::binding`
//!   supply committed misses or canonical eager exits, respectively.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{
    JitCompileSnapshot,
    jit::BindingHitProof,
    object::{FieldLocation, ShapeState},
};

use super::{
    fields::emit_field_base,
    values::{emit_load_symbol_u64, emit_load_u64},
};
use crate::entry::{
    GLOBAL_THIS_OFFSET_PTR_OFFSET, THREAD_OFFSET, VALUE_HOLE, VM_THREAD_ACTIVE_REALM_CELL_OFFSET,
    VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET,
};
use crate::{
    Unsupported,
    artifact::relocation::{RelocationCapture, RelocationTarget},
};

fn validate_temps([header, base]: [u8; 2]) {
    debug_assert_ne!(header, base);
    debug_assert!([header, base].iter().all(|r| *r != 10 && *r != 11));
}

/// Prove that isolate-live global cells belong to the actual source body.
/// Refuse a missing realm owner before reading any lexical/object hit.
pub(crate) fn emit_global_realm_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
) {
    dynasm!(ops ; .arch x64
        ; mov r10, [r15 + THREAD_OFFSET as i32]
        ; mov r10, [r10 + VM_THREAD_ACTIVE_REALM_CELL_OFFSET as i32]
        ; test r10, r10 ; jz =>miss
        ; cmp DWORD [r10], view.literal_allocations.realm_id as i32 ; jne =>miss);
}

/// Prove the current declarative epoch and object layout; retain the live
/// decompressed receiver in `header` and cage base in r11 on success.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_global_object_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    shape: u64,
    dictionary: bool,
    epoch: u64,
    [header, base]: [u8; 2],
    miss: DynamicLabel,
) {
    validate_temps([header, base]);
    emit_global_realm_guard(ops, view, miss);
    dynasm!(ops ; .arch x64
        ; mov Rq(base), [r15 + THREAD_OFFSET as i32]
        ; mov Rq(base), [Rq(base) + VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET as i32]
        ; test Rq(base), Rq(base) ; jz =>miss);
    emit_load_u64(ops, 11, epoch);
    dynasm!(ops ; .arch x64
        ; cmp [Rq(base)], r11 ; jne =>miss
        ; mov Rq(base), [r15 + GLOBAL_THIS_OFFSET_PTR_OFFSET as i32]
        ; mov Rd(header), [Rq(base)]
        ; test Rd(header), Rd(header) ; jz =>miss);
    emit_load_symbol_u64(
        ops,
        relocations,
        11,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch x64
        ; add Rq(header), r11
        ; mov Rd(base), [Rq(header) + view.object_shape_byte as i32]);
    if dictionary {
        dynasm!(ops ; .arch x64
            ; test BYTE [r11 + Rq(base) + view.shape_state_byte as i32], ShapeState::DICTIONARY_MASK as i8 ; jz =>miss
            ; test BYTE [r11 + Rq(base) + view.shape_state_byte as i32], (ShapeState::OPAQUE_LOOKUP_MASK | ShapeState::PROVISIONAL_MASK) as i8 ; jnz =>miss
            ; mov Rd(base), [Rq(header) + view.object_exotic_handle_byte as i32]
            ; test Rd(base), Rd(base) ; jz =>miss
            ; mov Rd(base), [r11 + Rq(base) + view.exotic_dictionary_layout_byte as i32]);
    }
    // Ordinary identity fixes state, descriptors and prototype role. The
    // dictionary token fixes watched layout but deliberately does not fix state.
    emit_load_u64(ops, 10, shape);
    dynasm!(ops ; .arch x64 ; cmp Rd(base), r10d ; jne =>miss);
}

/// Select the current bank and prove its resident index before any access.
/// The preceding object guard supplies the cage in r11; preserve `header`.
pub(crate) fn emit_global_field_bank(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    header: u8,
    base: u8,
    field: FieldLocation,
    miss: DynamicLabel,
) {
    validate_temps([header, base]);
    if field.is_inline() {
        dynasm!(ops ; .arch x64
            ; mov r10d, [Rq(header) + view.object_shape_byte as i32]
            ; movzx r10d, BYTE [r11 + r10 + view.shape_inline_capacity_byte as i32]
            ; cmp r10d, field.index() as i32 ; jbe =>miss);
    } else {
        dynasm!(ops ; .arch x64
            ; mov Rd(base), [Rq(header) + view.field_layout.slab_handle_byte as i32]
            ; test Rd(base), Rd(base) ; jz =>miss
            ; add Rq(base), r11
            ; cmp DWORD [Rq(base) + view.field_layout.slab_capacity_byte as i32], field.index() as i32 ; jbe =>miss);
    }
    emit_field_base(ops, relocations, view, header, base, 10, field);
}

/// Materialize an admitted permanent cell. `false` means no hit was emitted;
/// the caller retains its original cold policy, including lexical existence.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_global_cell_address(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    cell_offset: u32,
    byte_pc: u32,
    address: u8,
    miss: DynamicLabel,
) -> bool {
    let Some(cell) = (view.cage_base != 0)
        .then(|| view.cage_base.checked_add(cell_offset as usize))
        .flatten()
    else {
        return false;
    };
    emit_global_realm_guard(ops, view, miss);
    emit_load_symbol_u64(
        ops,
        relocations,
        address,
        cell as u64,
        RelocationTarget::GlobalLexicalCell {
            function_id: view.code_block.id,
            byte_pc,
        },
    );
    true
}

/// Read the current cell into reserved r10 and reject TDZ before result/store.
pub(crate) fn emit_global_lexical_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    address: u8,
    miss: DynamicLabel,
) {
    debug_assert!(address != 10 && address != 11);
    dynasm!(ops ; .arch x64 ; mov r10, [Rq(address) + view.global_lexical_value_byte as i32]);
    emit_load_u64(ops, 11, VALUE_HOLE);
    dynasm!(ops ; .arch x64 ; cmp r10, r11 ; je =>miss);
}

/// Emit one eligible global read and return its guard-end code offset.
/// The destination can alias either assigned temporary: commit it last.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_global_read(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    proof: BindingHitProof,
    byte_pc: u32,
    destination: u8,
    [header, base]: [u8; 2],
    miss: DynamicLabel,
) -> Result<usize, Unsupported> {
    validate_temps([header, base]);
    debug_assert!(destination != 10 && destination != 11);
    match proof {
        BindingHitProof::GlobalLexical { cell_offset, .. } => {
            if !emit_global_cell_address(ops, relocations, view, cell_offset, byte_pc, base, miss) {
                dynasm!(ops ; .arch x64 ; jmp =>miss);
                return Ok(ops.offset().0);
            }
            emit_global_lexical_guard(ops, view, base, miss);
            let guard_end = ops.offset().0;
            dynasm!(ops ; .arch x64 ; mov Rq(destination), r10);
            Ok(guard_end)
        }
        BindingHitProof::GlobalObject {
            shape,
            dictionary,
            field,
            global_lexical_epoch,
            ..
        } => {
            if view.cage_base == 0 {
                dynasm!(ops ; .arch x64 ; jmp =>miss);
                return Ok(ops.offset().0);
            }
            let offset = i32::try_from(field.byte_offset())
                .map_err(|_| Unsupported::OperandShape("global field displacement"))?;
            emit_global_object_guard(
                ops,
                relocations,
                view,
                shape,
                dictionary,
                global_lexical_epoch,
                [header, base],
                miss,
            );
            emit_global_field_bank(ops, relocations, view, header, base, field, miss);
            let guard_end = ops.offset().0;
            dynasm!(ops ; .arch x64 ; mov Rq(destination), [Rq(base) + offset]);
            Ok(guard_end)
        }
    }
}

#[cfg(test)]
mod tests;
