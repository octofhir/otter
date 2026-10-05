//! Shared physical global-binding reads and guards for native AArch64 tiers.
//!
//! # Contents
//! - Permanent source-attributed lexical cell addressing and live TDZ guards.
//! - Current global object epoch, ordinary shape or dictionary-layout guards.
//! - Current field-bank selection and complete tagged reads.
//!
//! # Invariants
//! The VM's existing BindingHitProof and opcode schema own source and semantics.
//! Two caller-declared GPs plus x16/x17 and flags are the only scratch; the
//! result is written after all guards. Nothing allocates, collects or calls,
//! and no interior pointer survives a miss. Every global hit proves the
//! source realm against the existing live activation realm cell; the canonical
//! producer refuses foreign ambient specialization before inline publication.
//! Template owns its committed cold completion and write policies; Graph owns
//! eager source recovery. Existence of a lexical binding does not read its TDZ.
//!
//! # See also
//! - `otter_vm::interp::jit_compile` owns proof publication and realm admission.
//! - `crate::template::arm64::binding` owns binding policy and barriers.
//! - `crate::graph::arm64::binding` owns eager exits and inline source identity.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{
    JitCompileSnapshot,
    jit::BindingHitProof,
    object::{FieldLocation, ShapeState},
};

use crate::{
    Unsupported,
    artifact::relocation::{RelocationCapture, RelocationTarget},
    entry::{
        GLOBAL_THIS_OFFSET_PTR_OFFSET, THREAD_OFFSET, VALUE_HOLE,
        VM_THREAD_ACTIVE_REALM_CELL_OFFSET, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET,
    },
    template::arm64::values::{emit_load_symbol_u64, emit_load_u64},
};

fn registers([header, base]: [u8; 2]) {
    debug_assert!(header < 31 && base < 31 && header != base);
    debug_assert!(!matches!(header, 16 | 17) && !matches!(base, 16 | 17));
}

/// Refuse an ambient realm before any generated global binding hit. This is
/// the source body's existing realm scalar and the activation's live owner.
pub(crate) fn emit_global_realm_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    miss: DynamicLabel,
) {
    dynasm!(ops ; .arch aarch64
        ; ldr x16, [x20, THREAD_OFFSET]
        ; ldr x16, [x16, VM_THREAD_ACTIVE_REALM_CELL_OFFSET]
        ; cbz x16, =>miss ; ldr w16, [x16]);
    emit_load_u64(ops, 17, u64::from(view.literal_allocations.realm_id));
    dynasm!(ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>miss);
}

/// Select the permanent cell from its actual source body's proof. An unusable
/// compile input branches to the caller's pre-effect miss without a pointer.
pub(crate) fn emit_global_cell_address(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    cell_offset: u32,
    byte_pc: u32,
    base: u8,
    miss: DynamicLabel,
) -> bool {
    emit_global_realm_guard(ops, view, miss);
    let Some(cell) = (view.cage_base != 0)
        .then_some(view.cage_base)
        .and_then(|cage| cage.checked_add(cell_offset as usize))
    else {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
        return false;
    };
    emit_load_symbol_u64(
        ops,
        relocations,
        base,
        cell as u64,
        RelocationTarget::GlobalLexicalCell {
            function_id: view.code_block.id,
            byte_pc,
        },
    );
    true
}

/// Keep the cell in `base` and its complete non-hole value in x16. Template
/// writes use the same TDZ guard before their existing assignment policy.
pub(crate) fn emit_global_lexical_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    cell_offset: u32,
    byte_pc: u32,
    base: u8,
    miss: DynamicLabel,
) -> bool {
    if !emit_global_cell_address(ops, relocations, view, cell_offset, byte_pc, base, miss) {
        return false;
    }
    dynasm!(ops ; .arch aarch64 ; ldr x16, [X(base), view.global_lexical_value_byte]);
    emit_load_u64(ops, 17, VALUE_HOLE);
    dynasm!(ops ; .arch aarch64 ; cmp x16, x17 ; b.eq =>miss);
    true
}

/// Prove a live own-data global and keep its header as the store barrier
/// parent. Dictionary state and its watched descriptor layout remain dynamic.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_global_object_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    shape: u64,
    dictionary: bool,
    global_lexical_epoch: u64,
    temps @ [header, base]: [u8; 2],
    miss: DynamicLabel,
) {
    registers(temps);
    emit_global_realm_guard(ops, view, miss);
    if view.cage_base == 0 {
        dynasm!(ops ; .arch aarch64 ; b =>miss);
        return;
    }
    dynasm!(ops ; .arch aarch64
        ; ldr x16, [x20, THREAD_OFFSET]
        ; ldr x16, [x16, VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET] ; cbz x16, =>miss
        ; ldr x17, [x16]);
    emit_load_u64(ops, base, global_lexical_epoch);
    dynasm!(ops ; .arch aarch64 ; cmp x17, X(base) ; b.ne =>miss
        ; ldr x16, [x20, GLOBAL_THIS_OFFSET_PTR_OFFSET] ; ldr W(header), [x16] ; cbz W(header), =>miss);
    emit_load_symbol_u64(
        ops,
        relocations,
        17,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch aarch64 ; add X(header), x17, X(header)
        ; ldr w16, [X(header), view.object_shape_byte]);
    if dictionary {
        dynasm!(ops ; .arch aarch64 ; add x16, x17, x16
            ; ldrb w16, [x16, view.shape_state_byte]
            ; tst w16, #u32::from(ShapeState::DICTIONARY_MASK) ; b.eq =>miss);
        emit_load_u64(
            ops,
            17,
            u64::from(ShapeState::OPAQUE_LOOKUP_MASK | ShapeState::PROVISIONAL_MASK),
        );
        dynasm!(ops ; .arch aarch64 ; tst w16, w17 ; b.ne =>miss
            ; ldr w16, [X(header), view.object_exotic_handle_byte] ; cbz w16, =>miss);
        emit_load_symbol_u64(
            ops,
            relocations,
            17,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(ops ; .arch aarch64 ; add x16, x17, x16
            ; ldr w16, [x16, view.exotic_dictionary_layout_byte]);
    }
    emit_load_u64(ops, 17, shape);
    dynasm!(ops ; .arch aarch64 ; cmp w16, w17 ; b.ne =>miss);
}

/// Resolve the current shape-selected bank without changing its object header.
/// Full slot bounds and suffix presence are checked before a read or write.
pub(crate) fn emit_global_field_bank(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    header: u8,
    base: u8,
    field: FieldLocation,
    miss: DynamicLabel,
) {
    registers([header, base]);
    if field.is_inline() {
        emit_load_symbol_u64(
            ops,
            relocations,
            17,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(ops ; .arch aarch64 ; ldr w16, [X(header), view.object_shape_byte]
            ; add x16, x17, x16 ; ldrb w16, [x16, view.shape_inline_capacity_byte]);
        emit_load_u64(ops, 17, u64::from(field.index()));
        dynasm!(ops ; .arch aarch64 ; cmp w16, w17 ; b.ls =>miss
            ; add XSP(base), XSP(header), #view.field_layout.inline_values_byte);
    } else {
        dynasm!(ops ; .arch aarch64 ; ldr W(base), [X(header), view.field_layout.slab_handle_byte] ; cbz W(base), =>miss);
        emit_load_symbol_u64(
            ops,
            relocations,
            17,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        dynasm!(ops ; .arch aarch64 ; add X(base), x17, X(base)
            ; ldr w16, [X(base), view.field_layout.slab_capacity_byte]);
        emit_load_u64(ops, 17, u64::from(field.index()));
        dynasm!(ops ; .arch aarch64 ; cmp w16, w17 ; b.ls =>miss
            ; add XSP(base), XSP(base), #view.field_layout.slab_words_byte);
    }
}

/// Read one existing physical proof with its exact source relocation. Returns
/// the guard-end offset used by Template's existing code-map attribution.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_global_read(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    proof: BindingHitProof,
    byte_pc: u32,
    destination: u8,
    temps @ [header, base]: [u8; 2],
    miss: DynamicLabel,
) -> Result<usize, Unsupported> {
    registers(temps);
    debug_assert!(destination < 31 && !matches!(destination, 16 | 17));
    let guard_end = match proof {
        BindingHitProof::GlobalLexical { cell_offset, .. } => {
            if !emit_global_lexical_guard(ops, relocations, view, cell_offset, byte_pc, base, miss)
            {
                return Ok(ops.offset().0);
            }
            ops.offset().0
        }
        BindingHitProof::GlobalObject {
            shape,
            dictionary,
            field,
            global_lexical_epoch,
            ..
        } => {
            emit_global_object_guard(
                ops,
                relocations,
                view,
                shape,
                dictionary,
                global_lexical_epoch,
                temps,
                miss,
            );
            emit_global_field_bank(ops, relocations, view, header, base, field, miss);
            let guard_end = ops.offset().0;
            let byte = field.byte_offset();
            if byte <= 32760 {
                dynasm!(ops ; .arch aarch64 ; ldr x16, [X(base), byte]);
            } else {
                emit_load_u64(ops, 17, u64::from(byte));
                dynasm!(ops ; .arch aarch64 ; ldr x16, [X(base), x17]);
            }
            guard_end
        }
    };
    dynasm!(ops ; .arch aarch64 ; mov X(destination), x16);
    Ok(guard_end)
}

#[cfg(test)]
mod tests;
