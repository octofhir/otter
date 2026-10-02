//! Exception-raising transition emission.
//!
//! # Contents
//! - Calls to the VM-owned helper that raises a compiled exception opcode's
//!   exception and routes it through the frame's handler table.
//!
//! # Invariants
//! - The helper never asks generated code to replay the source opcode: the
//!   frame either resumes at its handler's canonical PC, published before the
//!   shared bailout epilogue runs, or propagates the raised value.
//!
//! # See also
//! - `otter_vm::RuntimeCall::exception_op`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::values::{emit_load_runtime_stub, emit_load_u64};
use crate::artifact::relocation::RelocationCapture;
use crate::entry::NATIVE_FRAME_PC_OFFSET;

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_exception_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    opcode: u8,
    arg0: u64,
    bail: DynamicLabel,
    propagate_throw: DynamicLabel,
    fatal: DynamicLabel,
) {
    let resume = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_u64(ops, 1, u64::from(opcode));
    emit_load_u64(ops, 2, arg0);
    emit_load_u64(ops, 3, 0);
    emit_load_u64(ops, 4, 0);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.variadic_entry(abi::STUB_JIT_EXCEPTION_OP),
        abi::STUB_JIT_EXCEPTION_OP,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cmp x1, abi::NativeResultStatus::SideExit as u32
        ; b.eq =>resume
        ; cmp x1, abi::NativeResultStatus::Throw as u32
        ; b.eq =>propagate_throw
        ; b =>fatal
        ; =>resume
        ; str w0, [x21, NATIVE_FRAME_PC_OFFSET]
        ; b =>bail
    );
}
