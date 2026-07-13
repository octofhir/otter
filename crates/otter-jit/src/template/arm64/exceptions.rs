//! Structured-exception transition emission.
//!
//! # Contents
//! - Uniform calls to the VM-owned exception semantic helper.
//! - Dynamic same-frame continuation publication.
//! - Normal return and thrown-error routing.
//!
//! # Invariants
//! - Every VM-success result represents a committed opcode; generated code
//!   either falls through, resumes at the returned canonical PC, or exits with
//!   its returned value. It never replays the source opcode.
//! - Dynamic resume PCs are written to the published NativeFrame before the
//!   shared bailout epilogue runs.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_exception_op`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::values::emit_load_u64;
use crate::entry::{
    NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET, STATUS_BAILED, STATUS_CONTINUE, STATUS_RETURNED,
};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_exception_op(
    ops: &mut Assembler,
    transitions: &crate::entry::TransitionTable,
    opcode: u8,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    bail: DynamicLabel,
    returned: DynamicLabel,
    threw: DynamicLabel,
) {
    let resume = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_u64(ops, 1, u64::from(opcode));
    emit_load_u64(ops, 2, arg0);
    emit_load_u64(ops, 3, arg1);
    emit_load_u64(ops, 4, arg2);
    emit_load_u64(
        ops,
        16,
        transitions.variadic_entry(abi::STUB_JIT_EXCEPTION_OP),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x1, STATUS_CONTINUE as u32
        ; b.eq =>done
        ; cmp x1, STATUS_RETURNED as u32
        ; b.eq =>returned
        ; cmp x1, STATUS_BAILED as u32
        ; b.eq =>resume
        ; b =>threw
        ; =>resume
        ; ldr x9, [x20, NATIVE_FRAME_OFFSET]
        ; str w0, [x9, NATIVE_FRAME_PC_OFFSET]
        ; b =>bail
        ; =>done
    );
}
