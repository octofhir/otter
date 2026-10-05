//! Canonical-home literal initialization on the shared x86 LAB.
//!
//! # Contents
//! - Static shaped objects and tagged/numeric dense two-cell arrays.
//! - Immutable input recipes and full shell/slab initialization.
//!
//! # Invariants
//! - Inputs are initialized tagged homes or immediate constants.
//! - Realm/limit misses precede effects. Fits cannot collect or call helpers.
//! - Every field, data word and hole bit precedes the single LAB publication.
//! - Only four declared GP temporaries, r10/r11, flags and XMM15 are clobbered.
//! - Fresh young cells need no initializing write barrier; the result is late.
//!
//! # See also
//! - `otter_vm::jit::JitLiteralAllocationPlans` owns geometry/source semantics.
//! - [`super`] owns the target LAB probe and allocation accounting.

use super::empty::emit_source_realm_guard;
use super::{emit_bump_probe, emit_count_allocation, emit_publish};
use crate::allocation::{AllocationValue, LabRegisters};
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{ALLOC_WINDOW_LAB_OFFSET, LAB_TOP_OFFSET, VALUE_HOLE, VALUE_UNDEFINED};
use crate::x86_64::values::{emit_load_symbol_u64, emit_load_u64};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::jit::{
    JitArrayLiteralAllocationPlan, JitDenseArrayAllocationPlan, JitObjectLiteralAllocationPlan,
};
use otter_vm::value::tag;

fn emit_load_input(ops: &mut Assembler, input: AllocationValue) {
    match input {
        AllocationValue::Register(register) => {
            dynasm!(ops ; .arch x64 ; mov Rq(10), Rq(register));
        }
        AllocationValue::Constant(value) => emit_load_u64(ops, 10, value),
        AllocationValue::StackByte(byte) => {
            let byte = i32::try_from(byte).expect("validated canonical tagged-home offset");
            dynasm!(ops ; .arch x64 ; mov r10, [rsp + byte]);
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
    debug_assert_eq!(regs.size, 11);
    debug_assert!(
        [regs.buffer, regs.candidate, regs.end, regs.scratch]
            .iter()
            .all(|register| ![10, 11].contains(register))
    );
    emit_source_realm_guard(ops, context, realm, regs.scratch, slow);
    emit_load_u64(ops, regs.size, u64::from(plan.cell_bytes));
    emit_bump_probe(ops, context, regs, slow);
    let candidate = regs.candidate;
    emit_load_u64(ops, 10, plan.header_word);
    dynasm!(ops ; .arch x64 ; mov [Rq(candidate)], r10);
    let start = plan
        .fields
        .inline_byte(otter_vm::object::FieldLocation::inline(0));
    for byte in (8..start).step_by(8) {
        dynasm!(ops ; .arch x64 ; mov QWORD [Rq(candidate) + byte as i32], 0);
    }
    dynasm!(ops ; .arch x64 ; mov DWORD [Rq(candidate) + plan.shape_byte as i32], plan.shape as i32);
    for index in 0..plan.inline_capacity {
        let byte = plan
            .fields
            .inline_byte(otter_vm::object::FieldLocation::inline(index));
        match inputs.get(index as usize) {
            Some(input) => emit_load_input(ops, *input),
            None => emit_load_u64(ops, 10, VALUE_UNDEFINED),
        }
        dynasm!(ops ; .arch x64 ; mov [Rq(candidate) + byte as i32], r10);
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
    debug_assert_eq!(regs.size, 11);
    debug_assert!(
        [regs.buffer, regs.candidate, regs.end, regs.scratch]
            .iter()
            .all(|register| ![10, 11].contains(register))
    );
    if realm != 0 {
        dynasm!(ops ; .arch x64 ; jmp =>slow);
        return;
    }
    emit_source_realm_guard(ops, context, realm, regs.scratch, slow);
    let tagged = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    // The canonical allocator chooses numeric storage for numbers or holes.
    // Classify before reservation; a miss leaves the complete LAB untouched.
    for input in inputs {
        let next = ops.new_dynamic_label();
        emit_load_input(ops, *input);
        emit_load_u64(ops, 11, VALUE_HOLE);
        dynasm!(ops ; .arch x64 ; cmp r10, r11 ; je =>next);
        emit_load_u64(ops, 11, tag::NUMBER_TAG);
        dynasm!(ops ; .arch x64 ; test r10, r11 ; jz =>tagged ; =>next);
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
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>tagged);
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
    dynasm!(ops ; .arch x64 ; =>done);
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
    dynasm!(ops ; .arch x64 ; lea Rq(slab), [Rq(shell) + plan.shell.cell_bytes as i32]);
    emit_load_u64(ops, 10, plan.shell.header_word);
    dynasm!(ops ; .arch x64 ; mov [Rq(shell)], r10);
    for byte in (8..plan.shell.cell_bytes).step_by(8) {
        dynasm!(ops ; .arch x64 ; mov QWORD [Rq(shell) + byte as i32], 0);
    }
    emit_load_u64(ops, 10, plan.slab_header_word);
    dynasm!(ops ; .arch x64 ; mov [Rq(slab)], r10);
    for byte in (8..plan.slab_bytes).step_by(8) {
        dynasm!(ops ; .arch x64 ; mov QWORD [Rq(slab) + byte as i32], 0);
    }
    emit_load_u64(ops, 10, u64::from(count));
    dynasm!(ops ; .arch x64
        ; mov [Rq(slab) + plan.capacity_byte as i32], r10d
        ; mov [Rq(slab) + plan.len_byte as i32], r10d
        ; mov [Rq(shell) + plan.shell_length_byte as i32], r10
        ; mov [Rq(shell) + plan.shell_len_byte as i32], r10d
        ; mov [Rq(shell) + plan.shell_capacity_byte as i32], r10d
        ; mov BYTE [Rq(slab) + plan.kind_byte as i32], plan.initial_kind as i8
        ; mov DWORD [Rq(slab) + plan.dirty_start_byte as i32], -1
    );
    for (index, input) in inputs.iter().enumerate() {
        let byte = (plan.data_byte + index as u32 * 8) as i32;
        emit_load_input(ops, *input);
        if plan.bitmap_words == 0 {
            dynasm!(ops ; .arch x64 ; mov [Rq(slab) + byte], r10);
            continue;
        }
        let hole = ops.new_dynamic_label();
        let double = ops.new_dynamic_label();
        let initialized = ops.new_dynamic_label();
        emit_load_u64(ops, 11, VALUE_HOLE);
        dynasm!(ops ; .arch x64 ; cmp r10, r11 ; je =>hole);
        emit_load_u64(ops, 11, tag::NUMBER_TAG);
        dynasm!(ops ; .arch x64
            ; mov Rq(regs.buffer), r10
            ; and Rq(regs.buffer), r11
            ; cmp Rq(regs.buffer), r11
            ; jne =>double
            ; cvtsi2sd xmm15, r10d
            ; movsd [Rq(slab) + byte], xmm15
            ; jmp =>initialized
            ; =>double
        );
        emit_load_u64(ops, 11, tag::DOUBLE_ENCODE_OFFSET);
        dynasm!(ops ; .arch x64
            ; sub r10, r11
            ; mov [Rq(slab) + byte], r10
            ; jmp =>initialized
            ; =>hole
        );
        let bitmap_byte = (plan.bitmap_byte + index as u32 / 64 * 8) as i32;
        emit_load_u64(ops, 10, 1u64 << (index % 64));
        dynasm!(ops ; .arch x64
            ; or [Rq(slab) + bitmap_byte], r10
            ; add DWORD [Rq(slab) + plan.hole_count_byte as i32], 1
            ; mov BYTE [Rq(slab) + plan.kind_byte as i32], 2
            ; =>initialized
        );
    }
    // ArrayBody traces the compressed slab handle and refreshes its full data
    // address after movement. No raw interior pointer outlives this fit.
    emit_load_symbol_u64(
        ops,
        relocations,
        10,
        cage_base,
        RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch x64
        ; mov r11, Rq(slab)
        ; sub r11, r10
        ; mov [Rq(shell) + plan.shell_slab_byte as i32], r11d
        ; lea r10, [Rq(slab) + plan.data_byte as i32]
        ; mov [Rq(shell) + plan.shell_data_byte as i32], r10
        ; mov r10b, [Rq(slab) + plan.kind_byte as i32]
        ; mov [Rq(shell) + plan.shell_kind_byte as i32], r10b
        ; mov Rq(regs.buffer), [Rq(context) + ALLOC_WINDOW_LAB_OFFSET as i32]
        ; mov [Rq(regs.buffer) + LAB_TOP_OFFSET as i32], Rq(regs.end)
    );
    emit_load_u64(ops, regs.size, u64::from(plan.shell.cell_bytes));
    emit_count_allocation(
        ops,
        context,
        plan.shell.header_word as u8,
        regs.size,
        regs.buffer,
    );
    emit_load_u64(ops, regs.size, u64::from(plan.slab_bytes));
    emit_count_allocation(
        ops,
        context,
        plan.slab_header_word as u8,
        regs.size,
        regs.buffer,
    );
}
