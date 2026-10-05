//! Activation pieces shared by every x86-64 tier's call entry.
//!
//! # Contents
//! - [`emit_object_receiver_test`] — the receiver a sloppy body binds as is.
//! - [`emit_lexical_this`] — an arrow closure's `this` and `new.target`.
//! - [`emit_object_test`] — the Object test of constructor completion.
//!
//! # Invariants
//! - Call ABI registers: `rdi` context, `rsi` callee, `rdx` receiver, `rcx`
//!   `new.target`, `r8` actual count, `r9` entered generation, the exact aligned
//!   actual span above the return address.
//! - The emitters here clobber only `r9`, `r10` and `r11` besides their
//!   outputs.
//! - Number classification shifts exactly the NUMBER_TAG high-bit range;
//!   bit 48 remains outside that range. Function IDs are then selected by
//!   their low 16-bit tag before OTHER_TAG rejects other immediates.
//! - Tag comparisons inspect the exact unsigned byte or low 16-bit tag;
//!   short immediates are used only when their signed extension is identical.
//!
//! # See also
//! - [`crate::call_linkage`] — the architecture-neutral call contract.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, value::tag};

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
/// every primitive cell is not. Clobbers `r10` and `r11`; the value cannot
/// occupy `r11`, which holds the classification masks.
pub(crate) fn emit_object_test(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    value: u8,
    object: DynamicLabel,
    primitive: DynamicLabel,
) {
    // R11 is the only scratch used before the final cell-byte read. Keep the
    // source unchanged, including when it occupies R10. The VM encoding makes
    // this shift exactly equivalent to testing NUMBER_TAG: bit 48 is not part
    // of that tag and must not reject a function-id word by itself.
    const _: () = assert!(tag::NUMBER_TAG == u64::MAX << 49);
    const _: () = assert!(tag::OTHER_TAG == 2);
    assert_ne!(
        value, 11,
        "object-test source aliases classification scratch"
    );
    dynasm!(ops
        ; .arch x64
        ; mov r11, Rq(value)
        ; shr r11, 49
        ; jnz =>primitive
        ; cmp Rw(value), WORD tag::FUNCTION_ID_TAG as i16
        ; je =>object
        ; test Rb(value), tag::OTHER_TAG as i8
        ; jnz =>primitive
        ; movzx r10d, BYTE [Rq(value)]
    );
    for tag in view.primitive_cell_type_tags {
        emit_compare_primitive_tag(ops, tag);
        dynasm!(ops ; .arch x64 ; je =>primitive);
    }
    dynasm!(ops ; .arch x64 ; jmp =>object);
}

/// Compare the zero-extended cell byte in `r10d` without sign-extending a
/// high unsigned tag. The low range has an equivalent shorter encoding.
fn emit_compare_primitive_tag(ops: &mut Assembler, tag: u8) {
    if let Ok(immediate) = i8::try_from(tag) {
        // dynasm widens a CMP BYTE operand to imm32. REX.B, opcode 83 and
        // register-direct ModRM /7 encode the exact signed imm8 operation.
        ops.extend([0x41, 0x83, 0xfa, immediate as u8]);
    } else {
        dynasm!(ops ; .arch x64 ; cmp r10d, DWORD i32::from(tag));
    }
}

#[cfg(test)]
#[path = "activation/tests.rs"]
mod tests;

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
