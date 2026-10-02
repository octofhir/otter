//! Shared AArch64 guard for monomorphic method targets.
//!
//! # Contents
//! - [`MethodGuardSite`] — receiver register plus VM-baked method identity.
//! - [`emit_method_guard`] — receiver/prototype/slot validation producing the
//!   exact current callable in a physical register.
//! - [`emit_method_shape_dispatch`] / [`emit_method_target`] — one receiver
//!   decode and a hidden-class switch over a whole candidate chain, then the
//!   selected candidate's chain, slot and identity proof.
//!
//! # Invariants
//! - The guard re-reads every mutable heap fact immediately before use.
//! - Every miss branches to the caller's pre-effect deopt exit.
//! - Accepted closures have the expected function id and carry neither runtime
//!   call setup nor bound-`this` state. A following generated frame publishes
//!   the exact callable as SELF; frameless inlining admits only bodies that
//!   never read it.
//! - The returned callable is a full tagged `Value`, never a compressed slot.
//! - One validity word proves an inherited lookup. Its holder is read through
//!   a retained root shape, whose prototype field follows moving GC.
//!
//! # See also
//! - [`otter_vm::JitMethodGuard`] — owned compile-time guard metadata.
//! - [`super::direct_call`] — generated callee-frame construction.

use crate::template::arm64::values::{CellTest, emit_cell_test};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{
    JitCompileSnapshot, closure::JS_CLOSURE_BODY_TYPE_TAG, jit::JitMethodGuard,
    value::tag as value_tag,
};

use crate::{
    artifact::relocation::RelocationCapture,
    entry::{NUMBER_TAG_HI16, OBJECT_BODY_TYPE_TAG, Unsupported},
    template::arm64::values::{emit_load_reg, emit_load_symbol_u64, emit_load_u64, emit_slab_base},
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
    emit_load_reg(ops, 9, site.receiver)?;
    emit_method_guard_from_tagged_register(
        ops,
        relocations,
        view,
        site.guard,
        9,
        callable_register,
        receiver_body_register,
        bail,
    )
}

/// Decode the receiver in `X(receiver)` once and branch to `hits[i]` when its
/// hidden class is `guards[i]`'s receiver shape; any other value reaches
/// `miss`.
///
/// The checks every candidate shares — a heap object body in ordinary lookup
/// state — run once, before the shape compares. On every hit `x13` holds the
/// receiver header, as [`emit_method_target`] expects.
pub(crate) fn emit_method_shape_dispatch(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    guards: &[&JitMethodGuard],
    receiver: u8,
    hits: &[DynamicLabel],
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    dynasm!(ops
        ; .arch aarch64
        ; mov x9, X(receiver)
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #0x2
        ; tst x9, x11
        ; b.ne =>miss
        // A cell value is its header's full address.
        ; mov x13, x9
        ; ldrb w14, [x13]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>miss
    );
    crate::template::arm64::ic_probe::emit_ordinary_lookup_state_guard(ops, view, 13, miss);
    dynasm!(ops ; .arch aarch64 ; ldr w14, [x13, view.object_shape_byte]);
    for (guard, &hit) in guards.iter().zip(hits) {
        emit_load_u64(ops, 15, u64::from(guard.recv_shape));
        dynasm!(ops ; .arch aarch64 ; cmp w14, w15 ; b.eq =>hit);
    }
    dynasm!(ops ; .arch aarch64 ; b =>miss);
    Ok(())
}

/// At a hit of [`emit_method_shape_dispatch`], prove the candidate's chain,
/// read its method slot and prove the method's identity, leaving the exact
/// callable in `X(callable_register)`.
///
/// The call site's identity cell holds the last method proved there, so a
/// repeated method costs one compare; any other value takes the full proof,
/// which caches it on success.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_method_target(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    guard: &JitMethodGuard,
    plan: otter_vm::jit::JitDirectCallPlan,
    call_pc: u32,
    callable_register: u8,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    if let Some(validity) = guard.prototype_validity {
        crate::template::arm64::values::emit_prototype_validity_guard(
            ops,
            relocations,
            validity,
            14,
            miss,
        );
        emit_load_symbol_u64(
            ops,
            relocations,
            12,
            view.cage_base as u64,
            crate::artifact::relocation::RelocationTarget::GcCageBase,
        );
        emit_load_u64(ops, 14, u64::from(guard.holder_root));
        dynasm!(ops ; .arch aarch64 ; add x14, x12, x14
            ; ldr w13, [x14, view.shape_prototype_byte] ; add x13, x12, x13);
    }
    emit_slab_base(ops, relocations, view, 13, 14);
    dynasm!(ops
        ; .arch aarch64
        ; cbz x13, =>miss
        ; ldr x9, [x13, guard.method_value_byte]
    );
    let proven = ops.new_dynamic_label();
    let cell = (plan.callee_cell != 0).then_some(
        crate::artifact::relocation::RelocationTarget::CalleeIdentityCell {
            function_id: plan.function_id,
            call_pc,
        },
    );
    if let Some(cell) = cell.clone() {
        let start = ops.offset().0;
        emit_load_u64(ops, 10, plan.callee_cell);
        relocations.record_mov_wide(start, ops.offset().0, 10, cell);
        dynasm!(ops
            ; .arch aarch64
            ; ldr x10, [x10]
            ; cmp x9, x10
            ; b.eq =>proven
        );
    }
    let guarded = ops.new_dynamic_label();
    emit_load_u64(ops, 10, value_tag::box_function_id(guard.method_fid));
    dynasm!(ops
        ; .arch aarch64
        ; cmp x9, x10
        ; b.eq =>guarded
        ; cbz x9, =>miss
    );
    emit_cell_test(ops, 9, 10, CellTest::IsNotCell, miss);
    dynasm!(ops
        ; .arch aarch64
        ; ldrb w11, [x9]
        ; cmp w11, JS_CLOSURE_BODY_TYPE_TAG as u32
        ; b.ne =>miss
        ; ldr w11, [x9, view.closure_call_layout.flags_byte]
    );
    let incompatible_flags =
        view.closure_call_layout.runtime_setup_flags | view.closure_call_layout.bound_this_flag;
    emit_load_u64(ops, 12, u64::from(incompatible_flags));
    dynasm!(ops
        ; .arch aarch64
        ; tst w11, w12
        ; b.ne =>miss
        ; ldr w11, [x9, view.closure_call_layout.function_id_byte]
    );
    emit_load_u64(ops, 12, u64::from(guard.method_fid));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w11, w12
        ; b.ne =>miss
        ; =>guarded
    );
    if let Some(cell) = cell {
        let start = ops.offset().0;
        emit_load_u64(ops, 10, plan.callee_cell);
        relocations.record_mov_wide(start, ops.offset().0, 10, cell);
        dynasm!(ops ; .arch aarch64 ; str x9, [x10]);
    }
    dynasm!(ops ; .arch aarch64 ; =>proven ; mov X(callable_register), x9);
    Ok(())
}

/// Re-read and validate one method target from an already-loaded tagged value.
///
/// Optimizing-tier SSA values need not live in the interpreter window, so this
/// entry accepts the physical register containing the receiver while sharing
/// the exact same shape/prototype/callable contract as baseline calls.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_method_guard_from_tagged_register(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    guard: &JitMethodGuard,
    tagged_receiver_register: u8,
    callable_register: u8,
    receiver_body_register: Option<u8>,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    if view.cage_base == 0 {
        return Err(Unsupported::OperandShape("method guard cage base"));
    }

    dynasm!(ops
        ; .arch aarch64
        ; mov x9, X(tagged_receiver_register)
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #0x2
        ; tst x9, x11
        ; b.ne =>bail
        // A cell value is its header's full address.
        ; mov x13, x9
        ; ldrb w14, [x13]
        ; cmp w14, OBJECT_BODY_TYPE_TAG
        ; b.ne =>bail
        ; ldr w14, [x13, view.object_shape_byte]
    );
    emit_load_u64(ops, 15, u64::from(guard.recv_shape));
    dynasm!(ops ; .arch aarch64 ; cmp w14, w15 ; b.ne =>bail);
    crate::template::arm64::ic_probe::emit_ordinary_lookup_state_guard(ops, view, 13, bail);
    if let Some(register) = receiver_body_register {
        dynasm!(ops ; .arch aarch64 ; mov X(register), x13);
    }

    if let Some(validity) = guard.prototype_validity {
        crate::template::arm64::values::emit_prototype_validity_guard(
            ops,
            relocations,
            validity,
            14,
            bail,
        );
        emit_load_symbol_u64(
            ops,
            relocations,
            12,
            view.cage_base as u64,
            crate::artifact::relocation::RelocationTarget::GcCageBase,
        );
        emit_load_u64(ops, 14, u64::from(guard.holder_root));
        dynasm!(ops ; .arch aarch64 ; add x14, x12, x14
            ; ldr w13, [x14, view.shape_prototype_byte] ; add x13, x12, x13);
    }

    emit_slab_base(ops, relocations, view, 13, 14);
    dynasm!(ops
        ; .arch aarch64
        ; cbz x13, =>bail
        ; ldr x9, [x13, guard.method_value_byte]
    );
    dynasm!(ops ; .arch aarch64 ; mov X(callable_register), x9);

    let guarded = ops.new_dynamic_label();
    emit_load_u64(ops, 10, value_tag::box_function_id(guard.method_fid));
    dynasm!(ops
        ; .arch aarch64
        ; cmp X(callable_register), x10
        ; b.eq =>guarded
        ; cbz X(callable_register), =>bail
    );
    emit_cell_test(ops, callable_register, 10, CellTest::IsNotCell, bail);
    dynasm!(ops
        ; .arch aarch64
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
    );
    dynasm!(ops
        ; .arch aarch64
        ; ldr w11, [X(callable_register), view.closure_call_layout.function_id_byte]
    );
    emit_load_u64(ops, 12, u64::from(guard.method_fid));
    dynasm!(ops
        ; .arch aarch64
        ; cmp w11, w12
        ; b.ne =>bail
        ; =>guarded
    );
    Ok(())
}
