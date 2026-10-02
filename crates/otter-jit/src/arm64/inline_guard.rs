//! Shared exact identity proof for frameless inline callees.
//!
//! # Contents
//! - Callable identity for a frameless inline body.
//! - [`emit_cached_identity`] — the same proof behind a call site's cache of
//!   the last callee it proved.
//! - Plain-call this binding for the Machine inline activation recipe.
//! - Explicit-receiver this binding for a spliced reduced `f.call`.
//!
//! # Invariants
//! - x9 holds the callable; x10..x12 and x14 are reserved scratch.
//! - Misses occur before callee effects. The guard proves function identity;
//!   a spliced body that reads its SELF reads the guarded callable value, so
//!   closures of one function id still see their own contexts.
//! - The Machine guard returns exact this in x12 without allocating a frame.
//! - An explicit receiver arrives in x12. A sloppy callee binds it only when
//!   it is an object (nullish binds the global object); a primitive needs
//!   `ToObject` and misses, as does a bound-`this` sloppy closure.

use crate::artifact::relocation::RelocationCapture;
use crate::entry::VALUE_UNDEFINED;
use crate::template::arm64::values::{CellTest, emit_cell_test, emit_load_u64};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{
    JitCompileSnapshot, JitDirectCallThisMode, closure::JS_CLOSURE_BODY_TYPE_TAG,
    value::tag as value_tag,
};

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
    let start = ops.offset().0;
    emit_load_u64(ops, 10, plan.callee_cell);
    relocations.record_mov_wide(start, ops.offset().0, 10, cell.clone());
    dynasm!(ops
        ; .arch aarch64
        ; ldr x10, [x10]
        ; cmp x9, x10
        ; b.eq =>proven
    );
    emit_inline_identity(ops, view, plan.function_id, bail);
    let start = ops.offset().0;
    emit_load_u64(ops, 10, plan.callee_cell);
    relocations.record_mov_wide(start, ops.offset().0, 10, cell);
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
        emit_load_sloppy_global_this(ops, relocations, view, context_register);
    }
    dynasm!(ops ; .arch aarch64 ; =>done);
}

/// `this` of a spliced reduced `f.call(receiver, ...)` (§10.2.1.2
/// OrdinaryCallBindThis): x9 holds the proven callable, x12 the receiver, and
/// x12 receives the binding. A lexical or bound `this` closure keeps its own.
pub(crate) fn emit_inline_explicit_this(
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
    emit_load_u64(ops, 10, u64::from(view.closure_call_layout.bound_this_flag));
    dynasm!(ops ; .arch aarch64 ; tst w11, w10 ; b.eq =>unbound);
    if this_mode == JitDirectCallThisMode::StrictOrLexical {
        dynasm!(ops ; .arch aarch64 ; ldr x12, [x9, view.closure_call_layout.bound_this_byte] ; b =>done);
    } else {
        dynasm!(ops ; .arch aarch64 ; b =>bail);
    }
    dynasm!(ops ; .arch aarch64 ; =>unbound);
    if this_mode != JitDirectCallThisMode::StrictOrLexical {
        let global_this = ops.new_dynamic_label();
        emit_load_u64(ops, 14, VALUE_UNDEFINED);
        dynasm!(ops ; .arch aarch64 ; cmp x12, x14 ; b.eq =>global_this);
        emit_load_u64(ops, 14, value_tag::VALUE_NULL);
        dynasm!(ops ; .arch aarch64 ; cmp x12, x14 ; b.eq =>global_this);
        super::emit_object_type_branch(
            ops,
            relocations,
            view,
            12,
            [10, 11, 14],
            done,
            bail,
        );
        dynasm!(ops ; .arch aarch64 ; =>global_this);
        emit_load_sloppy_global_this(ops, relocations, view, context_register);
    }
    dynasm!(ops ; .arch aarch64 ; =>done);
}

/// Load the active realm's GC-rooted global object as full `Value` bits into
/// `x12`.
///
/// No allocation or safepoint occurs between reading the compressed root slot
/// and consuming it.
fn emit_load_sloppy_global_this(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    context_register: u8,
) {
    dynasm!(ops
        ; .arch aarch64
        ; ldr x14, [X(context_register), crate::entry::GLOBAL_THIS_OFFSET_PTR_OFFSET]
        ; ldr w12, [x14]
    );
    let start = ops.offset().0;
    let cage_base = view.cage_base as u64;
    dynasm!(ops ; .arch aarch64 ; movz x14, (cage_base & 0xffff) as u32);
    for shift in [16u32, 32, 48] {
        let part = ((cage_base >> shift) & 0xffff) as u32;
        if part != 0 {
            dynasm!(ops ; .arch aarch64 ; movk x14, part, lsl shift);
        }
    }
    relocations.record_mov_wide(
        start,
        ops.offset().0,
        14,
        crate::artifact::relocation::RelocationTarget::GcCageBase,
    );
    dynasm!(ops ; .arch aarch64 ; orr x12, x14, x12);
}
