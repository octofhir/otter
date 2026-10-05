//! Activation pieces shared by every AArch64 tier's call entry.
//!
//! # Contents
//! - [`emit_object_receiver_test`] — the receiver a sloppy body binds as is.
//! - [`emit_lexical_this`] — an arrow closure's `this` and `new.target`.
//! - [`emit_object_test`] — the Object test of constructor completion.
//!
//! # Invariants
//! - Call ABI registers: `x0` context, `x1` callee, `x2` receiver, `x3`
//!   `new.target`, `x4` actual count, the exact aligned actual span at `sp`.
//! - The emitters here clobber only x5, x9 and x10 besides their outputs.
//!
//! # See also
//! - [`crate::call_linkage`] — the architecture-neutral call contract.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{JitCompileSnapshot, value::tag};

use crate::template::arm64::values::emit_load_u64;

pub(crate) use crate::call_linkage::EntryShape;

/// `x5 = 1` unless the receiver `x2` is an object, which binds as is.
pub(crate) fn emit_object_receiver_test(ops: &mut Assembler, view: &JitCompileSnapshot) {
    let bound = ops.new_dynamic_label();
    let convert = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; mov x5, xzr);
    emit_object_test(ops, view, 2, bound, convert);
    dynasm!(ops ; .arch aarch64 ; =>convert ; movz x5, 1 ; =>bound);
}

/// Branch to `object` when `X(value)` is an Object and to `primitive`
/// otherwise. A function-id immediate is an object; every other non-cell and
/// every primitive cell is not. Clobbers x9 and x10.
pub(crate) fn emit_object_test(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    value: u8,
    object: DynamicLabel,
    primitive: DynamicLabel,
) {
    let not_function_id = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; and x9, X(value), 0xffff);
    emit_load_u64(ops, 10, tag::FUNCTION_ID_TAG);
    dynasm!(ops ; .arch aarch64 ; cmp x9, x10 ; b.ne =>not_function_id);
    emit_load_u64(ops, 9, tag::NUMBER_TAG);
    dynasm!(ops ; .arch aarch64 ; tst X(value), x9 ; b.eq =>object ; =>not_function_id);
    emit_load_u64(ops, 9, tag::NOT_CELL_MASK);
    dynasm!(ops ; .arch aarch64 ; tst X(value), x9 ; b.ne =>primitive ; ldrb w9, [X(value)]);
    for tag in view.primitive_cell_type_tags {
        dynasm!(ops ; .arch aarch64 ; cmp w9, u32::from(tag) ; b.eq =>primitive);
    }
    dynasm!(ops ; .arch aarch64 ; b =>object);
}

/// An arrow closure's lexical `this` and `new.target` replace `x2`/`x3`.
pub(crate) fn emit_lexical_this(ops: &mut Assembler, view: &JitCompileSnapshot) {
    let done = ops.new_dynamic_label();
    let layout = view.closure_call_layout;
    emit_load_u64(ops, 9, tag::NOT_CELL_MASK);
    dynasm!(ops
        ; .arch aarch64
        ; tst x1, x9
        ; b.ne =>done
        ; ldr w9, [x1, layout.flags_byte]
    );
    emit_load_u64(ops, 10, u64::from(layout.bound_this_flag));
    dynasm!(ops ; .arch aarch64 ; tst w9, w10 ; b.eq =>done ; ldr x2, [x1, layout.bound_this_byte]);
    emit_load_u64(ops, 10, u64::from(layout.bound_new_target_flag));
    dynasm!(ops
        ; .arch aarch64
        ; tst w9, w10
        ; b.eq =>done
        ; ldr x3, [x1, layout.bound_new_target_byte]
        ; =>done
    );
}
