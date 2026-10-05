//! Native context allocation using the target's shared LAB.
//!
//! # Contents
//! - Prepared scope initialization and dynamic per-iteration context copies.
//! - Explicit value recipes with allocator-declared temporaries.
//!
//! # Invariants
//! - Type/extension/size/limit misses precede writes; fits cannot collect.
//! - Parent, scope identity and every slot precede the single publication.
//! - Copy source is reloaded after the probe, consumed locally, and never
//!   survives a collecting boundary as an interior pointer.
//!
//! # See also
//! - `super` owns the sole target probe/publication/accounting encoder.
//! - `otter_vm::context` owns the allocating semantic fallback.

use super::*;

pub(super) fn emit_context_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    value: AllocationValue,
    regs: LabRegisters,
    undefined: bool,
    slow: DynamicLabel,
) {
    let ready = ops.new_dynamic_label();
    emit_value(ops, regs.scratch, value);
    if undefined {
        crate::x86_64::values::emit_load_u64(ops, regs.size, VALUE_UNDEFINED);
        dynasm!(ops ; .arch x64 ; cmp Rq(regs.scratch), Rq(regs.size) ; je =>ready);
    }
    crate::x86_64::values::emit_load_u64(ops, regs.size, otter_vm::value::tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch x64 ; test Rq(regs.scratch), Rq(regs.size) ; jnz =>slow
        ; cmp BYTE [Rq(regs.scratch)], view.context_layout.type_tag as i8 ; jne =>slow ; =>ready);
}

pub(crate) fn emit_create_context(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: &JitContextAllocationPlan,
    context: u8,
    parent: AllocationValue,
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    validate_inputs(regs, &[parent]);
    emit_context_guard(ops, view, parent, regs, true, slow);
    let layout = view.context_layout;
    crate::x86_64::values::emit_load_u64(ops, regs.size, u64::from(plan.cell_bytes));
    emit_bump_probe(ops, context, regs, slow);
    crate::x86_64::values::emit_load_u64(ops, regs.scratch, plan.header_word);
    dynasm!(ops ; .arch x64 ; mov [Rq(regs.candidate)], Rq(regs.scratch));
    crate::x86_64::values::emit_load_u64(ops, regs.scratch, plan.body_word);
    dynasm!(ops ; .arch x64 ; mov [Rq(regs.candidate) + layout.scope_function_id_byte as i32], Rq(regs.scratch));
    emit_value(ops, regs.scratch, parent);
    dynasm!(ops ; .arch x64 ; mov [Rq(regs.candidate) + layout.parent_byte as i32], Rq(regs.scratch));
    for (index, &word) in plan.initial_words.iter().enumerate() {
        crate::x86_64::values::emit_load_u64(ops, regs.scratch, word);
        let byte = (layout.slots_byte + 8 * index as u32) as i32;
        dynasm!(ops ; .arch x64 ; mov [Rq(regs.candidate) + byte], Rq(regs.scratch));
    }
    emit_publish(ops, context, layout.type_tag, regs);
}

pub(crate) fn emit_copy_context(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    context: u8,
    source: AllocationValue,
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    validate_inputs(regs, &[source]);
    emit_context_guard(ops, view, source, regs, false, slow);
    let layout = view.context_layout;
    let copy = ops.new_dynamic_label();
    let copied = ops.new_dynamic_label();
    emit_value(ops, regs.buffer, source);
    dynasm!(ops ; .arch x64
        ; test WORD [Rq(regs.buffer) + layout.scope_index_byte as i32], (1u16 << JIT_CONTEXT_HAS_EXTENSION_BIT) as i16 ; jnz =>slow
        ; movzx Rd(regs.size), WORD [Rq(regs.buffer) + layout.slot_count_byte as i32]
        ; cmp Rd(regs.size), JIT_INLINE_CONTEXT_MAX_WORDS as i32 ; ja =>slow
        ; shl Rd(regs.size), 3 ; add Rd(regs.size), layout.slots_byte as i32
    );
    emit_bump_probe(ops, context, regs, slow);
    dynasm!(ops ; .arch x64 ; mov Rq(regs.scratch), Rq(regs.size) ; shl Rq(regs.scratch), 32
        ; or Rq(regs.scratch), JIT_YOUNG_CONTEXT_HEADER_WORD as i32 ; mov [Rq(regs.candidate)], Rq(regs.scratch));
    emit_value(ops, regs.buffer, source);
    dynasm!(ops ; .arch x64
        ; mov Rq(regs.size), [Rq(regs.buffer) + layout.scope_function_id_byte as i32]
        ; mov [Rq(regs.candidate) + layout.scope_function_id_byte as i32], Rq(regs.size)
        ; mov Rq(regs.size), [Rq(regs.buffer) + layout.parent_byte as i32]
        ; mov [Rq(regs.candidate) + layout.parent_byte as i32], Rq(regs.size)
        ; movzx Rd(regs.scratch), WORD [Rq(regs.buffer) + layout.slot_count_byte as i32]
        ; test Rd(regs.scratch), Rd(regs.scratch) ; jz =>copied
        ; shl Rq(regs.scratch), 3 ; add Rq(regs.scratch), layout.slots_byte as i32
        ; =>copy ; sub Rq(regs.scratch), 8
        ; mov Rq(regs.size), [Rq(regs.buffer) + Rq(regs.scratch)]
        ; mov [Rq(regs.candidate) + Rq(regs.scratch)], Rq(regs.size)
        ; cmp Rq(regs.scratch), layout.slots_byte as i32 ; ja =>copy
        ; =>copied
        ; mov Rq(regs.buffer), [Rq(context) + ALLOC_WINDOW_LAB_OFFSET as i32]
        ; mov Rd(regs.size), [Rq(regs.candidate) + 4]
    );
    emit_publish(ops, context, layout.type_tag, regs);
}
