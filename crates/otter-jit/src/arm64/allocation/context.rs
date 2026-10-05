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
use otter_vm::jit::{JIT_CONTEXT_HAS_EXTENSION_BIT, JIT_INLINE_CONTEXT_MAX_WORDS};

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
        emit_load_u64(ops, regs.size, VALUE_UNDEFINED);
        dynasm!(ops ; .arch aarch64 ; cmp X(regs.scratch), X(regs.size) ; b.eq =>ready);
    }
    emit_cell_test(ops, regs.scratch, regs.size, CellTest::IsNotCell, slow);
    dynasm!(ops ; .arch aarch64 ; ldrb W(regs.size), [X(regs.scratch)] ; cmp WSP(regs.size), u32::from(view.context_layout.type_tag) ; b.ne =>slow ; =>ready);
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
    emit_load_u64(ops, regs.size, u64::from(plan.cell_bytes));
    emit_bump_probe(ops, context, regs, slow);
    emit_load_u64(ops, regs.scratch, plan.header_word);
    dynasm!(ops ; .arch aarch64 ; str X(regs.scratch), [X(regs.candidate)]);
    emit_load_u64(ops, regs.scratch, plan.body_word);
    dynasm!(ops ; .arch aarch64 ; str X(regs.scratch), [X(regs.candidate), layout.scope_function_id_byte]);
    emit_value(ops, regs.scratch, parent);
    dynasm!(ops ; .arch aarch64 ; str X(regs.scratch), [X(regs.candidate), layout.parent_byte]);
    for (index, &word) in plan.initial_words.iter().enumerate() {
        emit_load_u64(ops, regs.scratch, word);
        let byte = layout.slots_byte + 8 * index as u32;
        dynasm!(ops ; .arch aarch64 ; str X(regs.scratch), [X(regs.candidate), byte]);
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
    dynasm!(ops ; .arch aarch64
        ; ldrh W(regs.scratch), [X(regs.buffer), layout.scope_index_byte]
        ; tst W(regs.scratch), 1u32 << JIT_CONTEXT_HAS_EXTENSION_BIT ; b.ne =>slow
        ; ldrh W(regs.size), [X(regs.buffer), layout.slot_count_byte]
        ; cmp WSP(regs.size), JIT_INLINE_CONTEXT_MAX_WORDS as u32 ; b.hi =>slow
        ; lsl W(regs.size), W(regs.size), 3
        ; add WSP(regs.size), WSP(regs.size), layout.slots_byte
    );
    emit_bump_probe(ops, context, regs, slow);
    emit_load_u64(ops, regs.scratch, JIT_YOUNG_CONTEXT_HEADER_WORD);
    dynasm!(ops ; .arch aarch64 ; orr X(regs.scratch), X(regs.scratch), X(regs.size), lsl 32 ; str X(regs.scratch), [X(regs.candidate)]);
    emit_value(ops, regs.buffer, source);
    dynasm!(ops ; .arch aarch64
        ; ldr X(regs.size), [X(regs.buffer), layout.scope_function_id_byte]
        ; str X(regs.size), [X(regs.candidate), layout.scope_function_id_byte]
        ; ldr X(regs.size), [X(regs.buffer), layout.parent_byte]
        ; str X(regs.size), [X(regs.candidate), layout.parent_byte]
        ; ldrh W(regs.scratch), [X(regs.buffer), layout.slot_count_byte]
        ; cbz W(regs.scratch), =>copied
        ; lsl X(regs.scratch), X(regs.scratch), 3
        ; add XSP(regs.scratch), XSP(regs.scratch), layout.slots_byte
        ; =>copy
        ; sub XSP(regs.scratch), XSP(regs.scratch), 8
        ; ldr X(regs.size), [X(regs.buffer), X(regs.scratch)]
        ; str X(regs.size), [X(regs.candidate), X(regs.scratch)]
        ; cmp XSP(regs.scratch), layout.slots_byte ; b.hi =>copy
        ; =>copied
        ; ldr X(regs.buffer), [X(context), ALLOC_WINDOW_LAB_OFFSET]
        ; ldr W(regs.size), [X(regs.candidate), 4]
    );
    emit_publish(ops, context, layout.type_tag, regs);
}
