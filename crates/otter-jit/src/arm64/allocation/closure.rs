//! Explicit lexical closure initialization on the shared LAB.
//!
//! # Contents
//! - Prepared callable identity and rooted context/this/new.target recipes.
//!
//! # Invariants
//! - The encoder never reads physical-frame lexical bindings.
//! - All misses precede effects; every payload word precedes publication.
//! - Only declared LAB temporaries change; the caller commits the result late.
//!
//! # See also
//! - `otter_vm::function_ops` owns the common allocating semantic kernel.
//! - `super::context` owns the shared context-input guard.

use super::*;

pub(crate) fn emit_closure(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    plan: JitClosureAllocationPlan,
    context: u8,
    values: [AllocationValue; 3],
    regs: LabRegisters,
    slow: DynamicLabel,
) {
    validate_inputs(regs, &values);
    super::context::emit_context_guard(ops, view, values[0], regs, true, slow);
    let layout = view.closure_call_layout;
    let base_bytes = JIT_CLOSURE_CELL_BYTES + if plan.arrow { 8 } else { 0 };
    emit_load_u64(ops, regs.size, u64::from(base_bytes));
    if plan.arrow {
        let sized = ops.new_dynamic_label();
        emit_value(ops, regs.scratch, values[2]);
        emit_load_u64(ops, regs.buffer, VALUE_UNDEFINED);
        dynasm!(ops ; .arch aarch64 ; cmp X(regs.scratch), X(regs.buffer) ; b.eq =>sized ; add XSP(regs.size), XSP(regs.size), 8 ; =>sized);
    }
    emit_bump_probe(ops, context, regs, slow);
    emit_load_u64(ops, regs.scratch, JIT_YOUNG_CLOSURE_HEADER_WORD);
    dynasm!(ops ; .arch aarch64 ; orr X(regs.scratch), X(regs.scratch), X(regs.size), lsl 32 ; str X(regs.scratch), [X(regs.candidate)]);
    emit_load_u64(ops, regs.scratch, plan.call_word);
    dynasm!(ops ; .arch aarch64 ; str X(regs.scratch), [X(regs.candidate), layout.function_id_byte]);
    if plan.arrow {
        let ready = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64 ; cmp XSP(regs.size), JIT_CLOSURE_CELL_BYTES + 8 ; b.eq =>ready);
        emit_load_u64(
            ops,
            regs.scratch,
            plan.call_word | JitClosureAllocationPlan::bound_new_target_word(),
        );
        dynasm!(ops ; .arch aarch64 ; str X(regs.scratch), [X(regs.candidate), layout.function_id_byte]);
        emit_value(ops, regs.scratch, values[2]);
        dynasm!(ops ; .arch aarch64 ; str X(regs.scratch), [X(regs.candidate), layout.bound_new_target_byte] ; =>ready);
        emit_value(ops, regs.scratch, values[1]);
        dynasm!(ops ; .arch aarch64 ; str X(regs.scratch), [X(regs.candidate), layout.bound_this_byte]);
    }
    emit_value(ops, regs.scratch, values[0]);
    dynasm!(ops ; .arch aarch64 ; str X(regs.scratch), [X(regs.candidate), layout.context_byte] ; str xzr, [X(regs.candidate), layout.rare_byte]);
    emit_publish(
        ops,
        context,
        otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG,
        regs,
    );
}
