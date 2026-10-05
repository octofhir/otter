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
    crate::x86_64::values::emit_load_u64(ops, regs.size, u64::from(base_bytes));
    if plan.arrow {
        let sized = ops.new_dynamic_label();
        emit_value(ops, regs.scratch, values[2]);
        crate::x86_64::values::emit_load_u64(ops, regs.buffer, VALUE_UNDEFINED);
        dynasm!(ops ; .arch x64 ; cmp Rq(regs.scratch), Rq(regs.buffer) ; je =>sized ; add Rq(regs.size), 8 ; =>sized);
    }
    emit_bump_probe(ops, context, regs, slow);
    dynasm!(ops ; .arch x64 ; mov Rq(regs.scratch), Rq(regs.size) ; shl Rq(regs.scratch), 32 ; or Rq(regs.scratch), JIT_YOUNG_CLOSURE_HEADER_WORD as i32 ; mov [Rq(regs.candidate)], Rq(regs.scratch));
    crate::x86_64::values::emit_load_u64(ops, regs.scratch, plan.call_word);
    dynasm!(ops ; .arch x64 ; mov [Rq(regs.candidate) + layout.function_id_byte as i32], Rq(regs.scratch));
    if plan.arrow {
        let ready = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64 ; cmp Rq(regs.size), (JIT_CLOSURE_CELL_BYTES + 8) as i32 ; je =>ready);
        crate::x86_64::values::emit_load_u64(
            ops,
            regs.scratch,
            plan.call_word | JitClosureAllocationPlan::bound_new_target_word(),
        );
        dynasm!(ops ; .arch x64 ; mov [Rq(regs.candidate) + layout.function_id_byte as i32], Rq(regs.scratch));
        emit_value(ops, regs.scratch, values[2]);
        dynasm!(ops ; .arch x64 ; mov [Rq(regs.candidate) + layout.bound_new_target_byte as i32], Rq(regs.scratch) ; =>ready);
        emit_value(ops, regs.scratch, values[1]);
        dynasm!(ops ; .arch x64 ; mov [Rq(regs.candidate) + layout.bound_this_byte as i32], Rq(regs.scratch));
    }
    emit_value(ops, regs.scratch, values[0]);
    dynasm!(ops ; .arch x64 ; mov [Rq(regs.candidate) + layout.context_byte as i32], Rq(regs.scratch) ; mov QWORD [Rq(regs.candidate) + layout.rare_byte as i32], 0);
    emit_publish(
        ops,
        context,
        otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG,
        regs,
    );
}
