//! Shared AArch64 guard for monomorphic method targets.
//!
//! # Contents
//! - [`MethodGuardSite`] — receiver register plus VM-baked method identity.
//! - [`emit_method_guard`] — receiver/prototype/slot validation producing the
//!   exact current callable in a physical register.
//!
//! # Invariants
//! - The guard re-reads every mutable heap fact immediately before use.
//! - Every miss branches to the caller's pre-effect deopt exit.
//! - Accepted closures have the expected function id and carry neither runtime
//!   call setup nor bound-`this` state.
//! - The returned callable is a full tagged `Value`, never a compressed slot.
//!
//! # See also
//! - [`otter_vm::JitMethodGuard`] — owned compile-time guard metadata.
//! - [`super::direct_call`] — generated callee-frame construction.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{
    JitCompileSnapshot, closure::JS_CLOSURE_BODY_TYPE_TAG, jit::JitMethodGuard,
    value::tag as value_tag,
};

use crate::{
    artifact::relocation::RelocationCapture,
    entry::{NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG, Unsupported},
    template::arm64::values::{
        emit_decompress_slot, emit_load_reg, emit_load_symbol_u64, emit_load_u64, emit_slab_base,
    },
};

/// One receiver register and its exact monomorphic method identity.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MethodGuardSite<'a> {
    pub(crate) guard: &'a JitMethodGuard,
    pub(crate) receiver: u16,
}

/// Re-read and validate one method target.
///
/// `callable_register` receives the full current callable. When requested,
/// `receiver_body_register` retains the shape-guarded receiver body pointer for
/// a following call-free inline body.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_method_guard(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    site: MethodGuardSite<'_>,
    callable_register: u8,
    receiver_body_register: Option<u8>,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    if view.cage_base == 0 {
        return Err(Unsupported::OperandShape("method guard cage base"));
    }

    emit_load_reg(ops, 9, site.receiver)?;
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #0x2
        ; tst x9, x11
        ; b.ne =>bail
        ; mov w12, w9
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        13,
        view.cage_base as u64,
        crate::artifact::relocation::RelocationTarget::GcCageBase,
    );
    dynasm!(ops
        ; .arch aarch64
        ; add x13, x13, x12
        ; ldrb w14, [x13]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>bail
        ; ldr w14, [x13, view.object_shape_byte]
    );
    emit_load_u64(ops, 15, u64::from(site.guard.recv_shape));
    dynasm!(ops ; .arch aarch64 ; cmp w14, w15 ; b.ne =>bail);
    if let Some(register) = receiver_body_register {
        dynasm!(ops ; .arch aarch64 ; mov X(register), x13);
    }

    for &hop_shape in &site.guard.proto_chain {
        dynasm!(ops
            ; .arch aarch64
            ; ldr w9, [x13, view.jit_proto_byte]
            ; cbz w9, =>bail
        );
        emit_load_symbol_u64(
            ops,
            relocations,
            12,
            view.cage_base as u64,
            crate::artifact::relocation::RelocationTarget::GcCageBase,
        );
        dynasm!(ops
            ; .arch aarch64
            ; add x13, x12, x9
            ; ldrb w14, [x13]
            ; cmp w14, OBJECT_BODY_TYPE_TAG
            ; b.ne =>bail
            ; ldr w14, [x13, view.object_shape_byte]
        );
        emit_load_u64(ops, 15, u64::from(hop_shape));
        dynasm!(ops ; .arch aarch64 ; cmp w14, w15 ; b.ne =>bail);
    }

    emit_slab_base(ops, view, 13, 14);
    dynasm!(ops
        ; .arch aarch64
        ; cbz x13, =>bail
        ; ldr w9, [x13, site.guard.method_value_byte]
    );
    emit_decompress_slot(ops, relocations, view.cage_base as u64, bail);
    dynasm!(ops ; .arch aarch64 ; mov X(callable_register), x9);

    let closure = ops.new_dynamic_label();
    let guarded = ops.new_dynamic_label();
    emit_load_u64(ops, 10, value_tag::box_function_id(site.guard.method_fid));
    dynasm!(ops
        ; .arch aarch64
        ; cmp X(callable_register), x10
        ; b.eq =>guarded
        ; cbz X(callable_register), =>bail
    );
    emit_load_u64(ops, 10, value_tag::NOT_CELL_MASK);
    dynasm!(ops
        ; .arch aarch64
        ; tst X(callable_register), x10
        ; b.eq =>closure
        ; b =>bail
        ; =>closure
        ; ldrb w11, [X(callable_register)]
        ; cmp w11, JS_CLOSURE_BODY_TYPE_TAG as u32
        ; b.ne =>bail
        ; ldr w11, [X(callable_register), view.closure_call_layout.flags_byte]
    );
    let incompatible_flags =
        view.closure_call_layout.runtime_setup_flags | view.closure_call_layout.bound_this_flag;
    emit_load_u64(ops, 12, u64::from(incompatible_flags));
    dynasm!(ops
        ; .arch aarch64
        ; tst w11, w12
        ; b.ne =>bail
        ; ldr w11, [X(callable_register), view.closure_call_layout.function_id_byte]
    );
    emit_load_u64(ops, 12, u64::from(site.guard.method_fid));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w11, w12
        ; b.ne =>bail
        ; =>guarded
    );
    Ok(())
}
