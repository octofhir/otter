//! Shared exact identity proof for frameless inline callees.
//!
//! # Contents
//! - Callable identity for a frameless inline body.
//! - [`emit_cached_identity`] — the same proof behind a call site's cache of
//!   the last callee it proved.
//!
//! # Invariants
//! - x9 holds the callable; x10..x12 and x14 are reserved scratch.
//! - Misses occur before callee effects. The guard proves function identity;
//!   a spliced body that reads its SELF reads the guarded callable value, so
//!   closures of one function id still see their own contexts.

use crate::artifact::relocation::RelocationCapture;
use crate::template::arm64::values::{
    CellTest, emit_cell_test, emit_load_symbol_u64, emit_load_u64,
};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, closure::JS_CLOSURE_BODY_TYPE_TAG, value::tag as value_tag};

/// Prove the callee in `x9` is `plan`'s function or branch to `bail`.
///
/// The site's identity cell holds the last callee proved here, so a repeated
/// callee costs one compare; any other value takes the full proof, which
/// caches it on success. Clobbers `x10`–`x12`.
pub(crate) fn emit_cached_identity(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    plan: otter_vm::jit::JitDirectCallPlan,
    call_pc: u32,
    bail: DynamicLabel,
) {
    if plan.callee_cell == 0 {
        emit_inline_identity(ops, view, plan.function_id, bail);
        return;
    }
    let cell = crate::artifact::relocation::RelocationTarget::CalleeIdentityCell {
        function_id: plan.function_id,
        call_pc,
    };
    let proven = ops.new_dynamic_label();
    emit_load_symbol_u64(ops, relocations, 10, plan.callee_cell, cell.clone());
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [x10]
        ; cmp x9, x10
        ; b.eq =>proven
    );
    emit_inline_identity(ops, view, plan.function_id, bail);
    emit_load_symbol_u64(ops, relocations, 10, plan.callee_cell, cell);
    dynasm!(ops ; .arch aarch64 ; str x9, [x10] ; =>proven);
}

pub(crate) fn emit_inline_identity(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    function_id: u32,
    bail: DynamicLabel,
) {
    let guarded = ops.new_dynamic_label();
    emit_load_u64(ops, 10, value_tag::box_function_id(function_id));
    dynasm!(ops
        ; .arch aarch64
        ; cmp x9, x10
        ; b.eq =>guarded
        ; cbz x9, =>bail
    );
    emit_cell_test(ops, 9, CellTest::IsNotCell, bail);
    dynasm!(ops
        ; .arch aarch64
        // Heap-cell Values already carry the full pointer. No cage relocation
        // belongs on this path.
        ; ldrb w11, [x9]
        ; cmp w11, JS_CLOSURE_BODY_TYPE_TAG as u32
        ; b.ne =>bail
    );
    let closure_flags_byte = view.closure_call_layout.flags_byte;
    let closure_fid_byte = view.closure_call_layout.function_id_byte;
    if view.closure_call_layout.runtime_setup_flags != 0 {
        dynasm!(ops ; .arch aarch64 ; ldr w11, [x9, closure_flags_byte]);
        emit_load_u64(
            ops,
            12,
            u64::from(view.closure_call_layout.runtime_setup_flags),
        );
        dynasm!(ops
            ; .arch aarch64
            ; tst w11, w12
            ; b.ne =>bail
        );
    }
    dynasm!(ops ; .arch aarch64 ; ldr w11, [x9, closure_fid_byte]);
    emit_load_u64(ops, 12, u64::from(function_id));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w11, w12
        ; b.ne =>bail
        ; =>guarded
    );
}
