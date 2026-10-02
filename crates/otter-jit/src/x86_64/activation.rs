//! Activation pieces shared by every x86-64 tier's call entry.
//!
//! # Contents
//! - [`emit_object_receiver_test`] — the receiver a sloppy body binds as is.
//! - [`emit_lexical_this`] — an arrow closure's `this` and `new.target`.
//! - [`emit_object_test`] — the Object test of constructor completion.
//!
//! # Invariants
//! - Call ABI registers: `rdi` context, `rsi` callee, `rdx` receiver, `rcx`
//!   `new.target`, `r8` actual count, `r9` entered generation, the padded
//!   actual span above the return address.
//! - The emitters here clobber only `r9`, `r10` and `r11` besides their
//!   outputs.
//!
//! # See also
//! - [`crate::call_linkage`] — the architecture-neutral call contract.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, value::tag};

pub(crate) use crate::call_linkage::EntryShape;

/// `r9 = 1` unless the receiver `rdx` is an object, which binds as is.
pub(crate) fn emit_object_receiver_test(ops: &mut Assembler, view: &JitCompileSnapshot) {
    let bound = ops.new_dynamic_label();
    let convert = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; xor r9d, r9d);
    emit_object_test(ops, view, 2, bound, convert);
    dynasm!(ops ; .arch x64 ; =>convert ; mov r9d, 1 ; =>bound);
}

/// Jump to `object` when `Rq(value)` is an Object and to `primitive`
/// otherwise. A function-id immediate is an object; every other non-cell and
/// every primitive cell is not. Clobbers `r10` and `r11`.
pub(crate) fn emit_object_test(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    value: u8,
    object: DynamicLabel,
    primitive: DynamicLabel,
) {
    let not_function_id = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch x64
        ; mov r10d, Rd(value)
        ; and r10d, 0xffff
        ; cmp r10d, tag::FUNCTION_ID_TAG as i32
        ; jne =>not_function_id
        ; mov r11, QWORD tag::NUMBER_TAG as i64
        ; test Rq(value), r11
        ; jz =>object
        ; =>not_function_id
        ; mov r11, QWORD tag::NOT_CELL_MASK as i64
        ; test Rq(value), r11
        ; jnz =>primitive
        ; movzx r10d, BYTE [Rq(value)]
    );
    for tag in view.primitive_cell_type_tags {
        dynasm!(ops ; .arch x64 ; cmp r10d, i32::from(tag) ; je =>primitive);
    }
    dynasm!(ops ; .arch x64 ; jmp =>object);
}

/// An arrow closure's lexical `this` and `new.target` replace `rdx`/`rcx`.
pub(crate) fn emit_lexical_this(ops: &mut Assembler, view: &JitCompileSnapshot) {
    let done = ops.new_dynamic_label();
    let layout = view.closure_call_layout;
    dynasm!(ops
        ; .arch x64
        ; mov r11, QWORD tag::NOT_CELL_MASK as i64
        ; test rsi, r11
        ; jnz =>done
        ; mov r10d, [rsi + layout.flags_byte as i32]
        ; test r10d, layout.bound_this_flag as i32
        ; jz =>done
        ; mov rdx, [rsi + layout.bound_this_byte as i32]
        ; test r10d, layout.bound_new_target_flag as i32
        ; jz =>done
        ; mov rcx, [rsi + layout.bound_new_target_byte as i32]
        ; =>done
    );
}
