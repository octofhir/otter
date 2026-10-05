//! x86-64 exception-raising transitions through the platform C boundary.
//!
//! # Contents
//! - Calls to the VM-owned helper that raises a compiled exception opcode's
//!   exception and routes it through the frame's handler table.
//!
//! # Invariants
//! - The helper never asks generated code to replay the source opcode: the
//!   frame either resumes at its handler's canonical PC, stored in the
//!   published `Frame` before the ordinary runtime-transition side exit, or
//!   propagates the raised value.
//! - Calls pass exactly five physical words including the context, obey the
//!   platform C ABI and consume the shared native-result pair in `rax`/`rdx`.
//!
//! # See also
//! - `crate::template::arm64::exceptions` — peer target implementation.
//! - `otter_vm::RuntimeCall::exception_op` — semantic owner.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::native_abi as abi;

use super::{emit_load_runtime_stub, emit_load_u64};
use crate::{artifact::relocation::RelocationCapture, entry::NATIVE_FRAME_PC_OFFSET};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_exception_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    opcode: u8,
    arg0: u64,
    side_exit: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) {
    let resume = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    emit_load_u64(ops, 6, u64::from(opcode));
    emit_load_u64(ops, 2, arg0);
    emit_load_u64(ops, 1, 0);
    emit_load_u64(ops, 8, 0);
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_EXCEPTION_OP),
        abi::STUB_JIT_EXCEPTION_OP,
    );
    crate::x86_64::call_abi::emit_variadic_call(ops, abi::STUB_JIT_EXCEPTION_OP, 5);
    dynasm!(ops
        ; .arch x64
        ; cmp edx, abi::NativeResultStatus::SideExit as i32
        ; je =>resume
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; =>resume
        ; mov [r14 + NATIVE_FRAME_PC_OFFSET as i32], eax
        ; jmp =>side_exit
    );
}
