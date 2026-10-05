//! Canonical-home initialization of ordinary literal cells on the shared LAB.
//!
//! # Contents
//! - Static shaped objects and tagged/numeric dense arrays.
//! - Read-only input recipes and complete two-cell initialization.
//!
//! # Invariants
//! Inputs are initialized tagged homes or immediate constants. Realm/limit
//! misses precede effects. All shell/slab fields, element words and hole bits
//! are initialized before the single cursor publication. Fits cannot collect;
//! fresh young cells need no initializing write barrier. Only the four declared
//! GP temporaries, x16/x17 and reserved d31 are clobbered.
//!
//! # See also
//! `otter_vm::jit::JitLiteralAllocationPlans` owns geometry and source semantics.

use super::empty::emit_source_realm_guard;
use super::{emit_bump_probe, emit_count_allocation, emit_publish};
use crate::allocation::{AllocationValue, LabRegisters};
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{VALUE_HOLE, VALUE_UNDEFINED};
use crate::template::arm64::values::{emit_load_symbol_u64, emit_load_u64};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::jit::{
    JitArrayLiteralAllocationPlan, JitDenseArrayAllocationPlan, JitObjectLiteralAllocationPlan,
};
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
    context: u8,
    realm: u32,
    plan: JitObjectLiteralAllocationPlan,
    inputs: &[AllocationValue],
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    assert_eq!(inputs.len(), plan.value_count as usize);
    emit_source_realm_guard(ops, context, realm, regs.scratch, slow);
    emit_load_u64(ops, regs.size, u64::from(plan.cell_bytes));
    emit_bump_probe(ops, context, regs, slow);
    let candidate = regs.candidate;
    emit_load_u64(ops, 16, plan.header_word);
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
    emit_publish(ops, context, plan.header_word as u8, regs);
}

pub(crate) fn emit_array_literal(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    context: u8,
    realm: u32,
    cage_base: u64,
    plan: JitArrayLiteralAllocationPlan,
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
    let tagged = ops.new_dynamic_label();
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
        inputs,
        regs,
        slow,
    );
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>tagged);
    emit_dense_array(
        ops,
        relocations,
        context,
        cage_base,
        plan.tagged,
        inputs,
        regs,
        slow,
    );
    dynasm!(ops ; .arch aarch64 ; =>done);
}

fn emit_dense_array(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    context: u8,
    cage_base: u64,
    plan: JitDenseArrayAllocationPlan,
    inputs: &[AllocationValue],
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    let shell = regs.candidate;
    let slab = regs.scratch;
    let count = inputs.len() as u32;
    emit_load_u64(
        ops,
        regs.size,
        u64::from(plan.shell.cell_bytes + plan.slab_bytes),
    );
    emit_bump_probe(ops, context, regs, slow);
    dynasm!(ops ; .arch aarch64 ; add XSP(slab), XSP(shell), plan.shell.cell_bytes);
    emit_load_u64(ops, 16, plan.shell.header_word);
    dynasm!(ops ; .arch aarch64 ; str x16, [X(shell)]);
    for byte in (8..plan.shell.cell_bytes).step_by(8) {
        dynasm!(ops ; .arch aarch64 ; str xzr, [X(shell), byte]);
    }
    emit_load_u64(ops, 16, plan.slab_header_word);
    dynasm!(ops ; .arch aarch64 ; str x16, [X(slab)]);
    for byte in (8..plan.slab_bytes).step_by(8) {
        dynasm!(ops ; .arch aarch64 ; str xzr, [X(slab), byte]);
    }
    emit_load_u64(ops, 16, u64::from(count));
    dynasm!(ops
        ; .arch aarch64
        ; str w16, [X(slab), plan.capacity_byte]
        ; str w16, [X(slab), plan.len_byte]
        ; str x16, [X(shell), plan.shell_length_byte]
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
        ; ldr X(regs.buffer), [X(context), crate::entry::ALLOC_WINDOW_LAB_OFFSET]
        ; str X(regs.end), [X(regs.buffer), crate::entry::LAB_TOP_OFFSET]
    );
    emit_load_u64(ops, regs.size, u64::from(plan.shell.cell_bytes));
    emit_count_allocation(
        ops,
        context,
        plan.shell.header_word as u8,
        regs.size,
        regs.buffer,
        regs.scratch,
    );
    emit_load_u64(ops, regs.size, u64::from(plan.slab_bytes));
    emit_count_allocation(
        ops,
        context,
        plan.slab_header_word as u8,
        regs.size,
        regs.buffer,
        regs.scratch,
    );
}

#[cfg(test)]
#[path = "literal_tests.rs"]
mod tests;
