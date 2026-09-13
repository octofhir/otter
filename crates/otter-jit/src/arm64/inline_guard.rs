//! Shared exact identity proof for frameless inline callees.
//!
//! # Contents
//! - Callable identity and unsupported dynamic-environment rejection.
//! - Plain-call this binding for the Machine inline activation recipe.
//!
//! # Invariants
//! - x9 holds the callable; x10..x12 and x14 are reserved scratch.
//! - Misses occur before callee effects. Dynamic eval environments are rejected.
//! - The Machine guard returns exact this in x12 without allocating a frame.

use crate::artifact::relocation::RelocationCapture;
use crate::entry::VALUE_UNDEFINED;
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_u64};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{
    JitCompileSnapshot, JitDirectCallThisMode, closure::JS_CLOSURE_BODY_TYPE_TAG,
    value::tag as value_tag,
};

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
    emit_cell_test(ops, 9, 10, CellTest::IsNotCell, bail);
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
    // Frameless leaf inlining has no callee NativeFrame slot to carry a
    // closure's dynamic eval chain. A generated call can propagate this
    // handle; an inline candidate must prove it null before entering.
    dynasm!(ops
        ; .arch aarch64
        ; ldr w11, [x9, view.closure_call_layout.eval_env_byte]
        ; cbnz w11, =>bail
    );
    dynasm!(ops ; .arch aarch64 ; ldr w11, [x9, closure_fid_byte]);
    emit_load_u64(ops, 12, u64::from(function_id));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w11, w12
        ; b.ne =>bail
        ; =>guarded
    );
}

pub(crate) fn emit_inline_this(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    this_mode: JitDirectCallThisMode,
    relocations: &mut RelocationCapture,
    context_register: u8,
    bail: DynamicLabel,
) {
    let unbound = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_cell_test(ops, 9, 10, CellTest::IsNotCell, unbound);
    dynasm!(ops ; .arch aarch64 ; ldr w11, [x9, view.closure_call_layout.flags_byte]);
    emit_load_u64(ops, 12, u64::from(view.closure_call_layout.bound_this_flag));
    dynasm!(ops ; .arch aarch64 ; tst w11, w12 ; b.eq =>unbound);
    if this_mode == JitDirectCallThisMode::StrictOrLexical {
        dynasm!(ops ; .arch aarch64 ; ldr x12, [x9, view.closure_call_layout.bound_this_byte] ; b =>done);
    } else {
        dynasm!(ops ; .arch aarch64 ; b =>bail);
    }
    dynasm!(ops ; .arch aarch64 ; =>unbound);
    if this_mode == JitDirectCallThisMode::StrictOrLexical {
        emit_load_u64(ops, 12, VALUE_UNDEFINED);
    } else {
        super::direct_call::emit_load_sloppy_global_this(ops, relocations, view, context_register);
    }
    dynasm!(ops ; .arch aarch64 ; =>done);
}
