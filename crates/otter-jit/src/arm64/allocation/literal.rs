//! Canonical-home initialization of ordinary literal cells on the shared LAB.
//!
//! # Contents
//! - Static shaped objects and tagged/numeric dense arrays.
//! - Array shells over a literal boilerplate's shared slab.
//! - Read-only input recipes and complete two-cell initialization.
//!
//! # Invariants
//! Inputs are initialized tagged homes or immediate constants. Realm/limit
//! misses precede effects. All shell/slab fields, element words and hole bits
//! are initialized before the single cursor publication. Fits cannot collect;
//! fresh young cells need no initializing write barrier. Only the four declared
//! GP temporaries, x16/x17 and reserved d31 are clobbered.
//! Each field's selected record comes from the Graph plan; an observing record
//! updates its defining summary through a noncollecting leaf on an inline
//! membership miss, before the cell becomes visible. An all-zero literal emits
//! no observation and keeps the LAB buffer register live to publication.
//!
//! # See also
//! `otter_vm::jit::JitLiteralAllocationPlans` owns geometry and source semantics.

use super::empty::emit_source_realm_guard;
use super::{emit_bump_probe_in, emit_publish};
use crate::allocation::{AllocationValue, LabRegisters};
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    ALLOC_WINDOW_LAB_OFFSET, ALLOC_WINDOW_OLD_LAB_OFFSET, VALUE_HOLE, VALUE_UNDEFINED,
};
use crate::field_store::FieldStoreOwner;
use crate::template::arm64::values::{emit_load_symbol_u64, emit_load_u64};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::jit::{
    JitAllocationSite, JitArrayLiteralAllocationPlan, JitBoilerplateArrayAllocationPlan,
    JitDenseArrayAllocationPlan, JitObjectLiteralAllocationPlan, JitSiteMode,
};

/// The window offset of the buffer a site's cells are carved from.
fn lab_offset(site: JitAllocationSite) -> u32 {
    if site.mode == JitSiteMode::Old {
        ALLOC_WINDOW_OLD_LAB_OFFSET
    } else {
        ALLOC_WINDOW_LAB_OFFSET
    }
}
use otter_vm::value::tag;

fn emit_load_input(ops: &mut Assembler, input: AllocationValue) {
    match input {
        AllocationValue::Register(register) => {
            dynasm!(ops ; .arch aarch64 ; mov X(16), X(register));
        }
        AllocationValue::Constant(value) => emit_load_u64(ops, 16, value),
        AllocationValue::StackByte(byte) if byte <= 32760 && byte.is_multiple_of(8) => {
            dynasm!(ops ; .arch aarch64 ; ldr x16, [sp, byte]);
        }
        AllocationValue::StackByte(byte) => {
            emit_load_u64(ops, 17, u64::from(byte));
            dynasm!(ops ; .arch aarch64 ; add x17, sp, x17 ; ldr x16, [x17]);
        }
    }
}

pub(crate) fn emit_object_literal(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &otter_vm::JitCompileSnapshot,
    context: u8,
    realm: u32,
    plan: &JitObjectLiteralAllocationPlan,
    site: JitAllocationSite,
    fields: &[FieldStoreOwner],
    inputs: &[AllocationValue],
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    assert_eq!(inputs.len(), plan.value_count as usize);
    assert_eq!(inputs.len(), fields.len());
    emit_source_realm_guard(ops, context, realm, regs.scratch, slow);
    emit_load_u64(ops, regs.size, u64::from(plan.cell_bytes));
    emit_bump_probe_in(ops, context, lab_offset(site), regs, slow);
    let candidate = regs.candidate;
    emit_load_u64(ops, 16, site.header_word(plan.header_word));
    dynasm!(ops ; .arch aarch64 ; str x16, [X(candidate)]);
    let start = plan
        .fields
        .inline_byte(otter_vm::object::FieldLocation::inline(0));
    for byte in (8..start).step_by(8) {
        dynasm!(ops ; .arch aarch64 ; str xzr, [X(candidate), byte]);
    }
    emit_load_u64(ops, 16, u64::from(plan.shape));
    dynasm!(ops ; .arch aarch64 ; str w16, [X(candidate), plan.shape_byte]);
    for index in 0..plan.inline_capacity {
        let byte = plan
            .fields
            .inline_byte(otter_vm::object::FieldLocation::inline(index));
        match inputs.get(index as usize) {
            Some(input) => emit_load_input(ops, *input),
            None => emit_load_u64(ops, 16, VALUE_UNDEFINED),
        }
        dynasm!(ops ; .arch aarch64 ; str x16, [X(candidate), byte]);
    }
    let mut observed = false;
    for (input, record) in inputs
        .iter()
        .zip(fields)
        .filter(|(_, record)| record.observes())
    {
        emit_load_input(ops, *input);
        dynasm!(ops ; .arch aarch64 ; mov X(regs.buffer), x16);
        emit_load_u64(ops, regs.scratch, u64::from(record.owner_shape));
        crate::arm64::field_type::emit_observe_known(
            ops,
            relocations,
            view,
            regs.scratch,
            regs.buffer,
            record,
        );
        observed = true;
    }
    if observed {
        dynasm!(ops ; .arch aarch64 ; ldr X(regs.buffer), [X(context), lab_offset(site)]);
    }
    emit_publish(ops, regs);
}

pub(crate) fn emit_array_literal(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    context: u8,
    realm: u32,
    cage_base: u64,
    plan: JitArrayLiteralAllocationPlan,
    site: JitAllocationSite,
    inputs: &[AllocationValue],
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    assert_eq!(inputs.len(), plan.value_count as usize);
    if realm != 0 {
        dynasm!(ops ; .arch aarch64 ; b =>slow);
        return;
    }
    emit_source_realm_guard(ops, context, realm, regs.scratch, slow);
    // A tenured array is carved only numeric: tagged elements would need
    // their old slab remembered, which the committed allocation owns.
    let tagged = if site.mode == JitSiteMode::Old {
        slow
    } else {
        ops.new_dynamic_label()
    };
    let done = ops.new_dynamic_label();
    // Match the canonical allocator's number-or-hole classification before
    // either reservation; classification itself cannot alter the heap.
    for input in inputs {
        let next = ops.new_dynamic_label();
        emit_load_input(ops, *input);
        emit_load_u64(ops, 17, VALUE_HOLE);
        dynasm!(ops ; .arch aarch64 ; cmp x16, x17 ; b.eq =>next);
        emit_load_u64(ops, 17, tag::NUMBER_TAG);
        dynasm!(ops ; .arch aarch64 ; tst x16, x17 ; b.eq =>tagged ; =>next);
    }
    emit_dense_array(
        ops,
        relocations,
        context,
        cage_base,
        plan.numeric,
        site,
        inputs,
        regs,
        slow,
    );
    if site.mode != JitSiteMode::Old {
        dynasm!(ops ; .arch aarch64 ; b =>done ; =>tagged);
        emit_dense_array(
            ops,
            relocations,
            context,
            cage_base,
            plan.tagged,
            site,
            inputs,
            regs,
            slow,
        );
    }
    dynasm!(ops ; .arch aarch64 ; =>done);
}

/// Carve one array shell over a literal boilerplate's shared slab. The slab
/// is old and immutable, so the shell owes no barrier wherever it is born.
pub(crate) fn emit_boilerplate_array(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    context: u8,
    realm: u32,
    cage_base: u64,
    plan: JitBoilerplateArrayAllocationPlan,
    site: JitAllocationSite,
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    if realm != 0 {
        dynasm!(ops ; .arch aarch64 ; b =>slow);
        return;
    }
    emit_source_realm_guard(ops, context, realm, regs.scratch, slow);
    emit_load_u64(ops, regs.size, u64::from(plan.shell.cell_bytes));
    emit_bump_probe_in(ops, context, lab_offset(site), regs, slow);
    let shell = regs.candidate;
    emit_load_u64(ops, 16, site.header_word(plan.shell.header_word));
    dynasm!(ops ; .arch aarch64 ; str x16, [X(shell)]);
    emit_load_u64(ops, 16, plan.shell.map_word);
    dynasm!(ops ; .arch aarch64 ; str x16, [X(shell), 8]);
    for byte in (16..plan.shell.cell_bytes).step_by(8) {
        dynasm!(ops ; .arch aarch64 ; str xzr, [X(shell), byte]);
    }
    emit_load_u64(ops, 16, u64::from(plan.len));
    dynasm!(ops
        ; .arch aarch64
        ; str w16, [X(shell), plan.shell_length_byte]
        ; str w16, [X(shell), plan.shell_len_byte]
    );
    emit_load_u64(ops, 16, u64::from(plan.slab));
    dynasm!(ops ; .arch aarch64 ; str w16, [X(shell), plan.shell_slab_byte]);
    emit_load_symbol_u64(
        ops,
        relocations,
        17,
        cage_base,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x16, x16, x17
        ; add x16, x16, plan.data_byte
        ; str x16, [X(shell), plan.shell_data_byte]
        ; mov w16, u32::from(plan.kind)
        ; strb w16, [X(shell), plan.shell_kind_byte]
        ; mov w16, u32::from(plan.own_guard)
        ; strb w16, [X(shell), plan.shell_own_guard_byte]
    );
    emit_publish(ops, regs);
}

fn emit_dense_array(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    context: u8,
    cage_base: u64,
    plan: JitDenseArrayAllocationPlan,
    site: JitAllocationSite,
    inputs: &[AllocationValue],
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    let shell = regs.candidate;
    let slab = regs.scratch;
    let count = inputs.len() as u32;
    // The shell carries the site; the slab is born where the shell is.
    let slab_site = JitAllocationSite {
        mode: if site.mode == JitSiteMode::Old {
            JitSiteMode::Old
        } else {
            JitSiteMode::Young
        },
        ..site
    };
    emit_load_u64(
        ops,
        regs.size,
        u64::from(plan.shell.cell_bytes + plan.slab_bytes),
    );
    emit_bump_probe_in(ops, context, lab_offset(site), regs, slow);
    dynasm!(ops ; .arch aarch64 ; add XSP(slab), XSP(shell), plan.shell.cell_bytes);
    emit_load_u64(ops, 16, site.header_word(plan.shell.header_word));
    dynasm!(ops ; .arch aarch64 ; str x16, [X(shell)]);
    emit_load_u64(ops, 16, plan.shell.map_word);
    dynasm!(ops ; .arch aarch64 ; str x16, [X(shell), 8]);
    for byte in (16..plan.shell.cell_bytes).step_by(8) {
        dynasm!(ops ; .arch aarch64 ; str xzr, [X(shell), byte]);
    }
    emit_load_u64(ops, 16, slab_site.header_word(plan.slab_header_word));
    dynasm!(ops ; .arch aarch64 ; str x16, [X(slab)]);
    for byte in (8..plan.slab_bytes).step_by(8) {
        dynasm!(ops ; .arch aarch64 ; str xzr, [X(slab), byte]);
    }
    emit_load_u64(ops, 16, u64::from(count));
    dynasm!(ops
        ; .arch aarch64
        ; str w16, [X(slab), plan.capacity_byte]
        ; str w16, [X(slab), plan.len_byte]
        ; str w16, [X(shell), plan.shell_length_byte]
        ; str w16, [X(shell), plan.shell_len_byte]
        ; str w16, [X(shell), plan.shell_capacity_byte]
    );
    emit_load_u64(ops, 16, u64::from(plan.initial_kind));
    dynasm!(ops ; .arch aarch64 ; strb w16, [X(slab), plan.kind_byte]);
    emit_load_u64(ops, 16, u64::from(u32::MAX));
    dynasm!(ops ; .arch aarch64 ; str w16, [X(slab), plan.dirty_start_byte]);
    if plan.bitmap_words != 0 {
        emit_load_u64(ops, regs.buffer, tag::NUMBER_TAG);
    }
    for (index, input) in inputs.iter().enumerate() {
        let byte = plan.data_byte + index as u32 * 8;
        emit_load_input(ops, *input);
        if plan.bitmap_words == 0 {
            dynasm!(ops ; .arch aarch64 ; str x16, [X(slab), byte]);
            continue;
        }
        let hole = ops.new_dynamic_label();
        let double = ops.new_dynamic_label();
        let initialized = ops.new_dynamic_label();
        emit_load_u64(ops, 17, VALUE_HOLE);
        dynasm!(ops ; .arch aarch64 ; cmp x16, x17 ; b.eq =>hole);
        dynasm!(ops
            ; .arch aarch64
            ; and x17, x16, X(regs.buffer)
            ; cmp x17, X(regs.buffer)
            ; b.ne =>double
            ; scvtf d31, w16
            ; str d31, [X(slab), byte]
            ; b =>initialized
            ; =>double
        );
        emit_load_u64(ops, 17, tag::DOUBLE_ENCODE_OFFSET);
        dynasm!(ops ; .arch aarch64 ; sub x16, x16, x17 ; str x16, [X(slab), byte] ; b =>initialized ; =>hole);
        let bitmap_byte = plan.bitmap_byte + index as u32 / 64 * 8;
        emit_load_u64(ops, 17, 1u64 << (index % 64));
        dynasm!(ops
            ; .arch aarch64
            ; ldr x16, [X(slab), bitmap_byte]
            ; orr x16, x16, x17
            ; str x16, [X(slab), bitmap_byte]
            ; ldr w16, [X(slab), plan.hole_count_byte]
            ; add w16, w16, #1
            ; str w16, [X(slab), plan.hole_count_byte]
            ; mov w16, #2
            ; strb w16, [X(slab), plan.kind_byte]
            ; =>initialized
        );
    }
    // Raw handles are compressed offsets; the cache remains a full data
    // address. Both are owned by ArrayBody's traced relocation refresh.
    emit_load_symbol_u64(
        ops,
        relocations,
        16,
        cage_base,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; sub x16, X(slab), x16
        ; str w16, [X(shell), plan.shell_slab_byte]
        ; add x16, XSP(slab), plan.data_byte
        ; str x16, [X(shell), plan.shell_data_byte]
        ; ldrb w16, [X(slab), plan.kind_byte]
        ; strb w16, [X(shell), plan.shell_kind_byte]
        ; ldr X(regs.buffer), [X(context), lab_offset(site)]
        ; str X(regs.end), [X(regs.buffer), crate::entry::LAB_TOP_OFFSET]
    );
}

#[cfg(test)]
#[path = "literal_tests.rs"]
mod tests;
