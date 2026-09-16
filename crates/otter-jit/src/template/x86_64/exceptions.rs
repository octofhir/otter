//! System V x86-64 structured-exception transition emission.
//!
//! # Contents
//! - Uniform calls to the VM-owned exception semantic helper.
//! - Dynamic same-frame continuation publication.
//! - Normal return and committed-throw routing.
//!
//! # Invariants
//! - The target-neutral template operation is committed exactly once by the
//!   shared runtime stub; generated code never replays it after reentry.
//! - A dynamic resume PC is stored in the published `NativeFrame` before the
//!   ordinary runtime-transition side exit.
//! - Calls obey the System V integer ABI and consume the shared native-result
//!   pair in `rax`/`rdx`.
//!
//! # See also
//! - `crate::template::arm64::exceptions` — peer target implementation.
//! - `otter_vm::Interpreter::jit_runtime_exception_op` — semantic owner.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::native_abi as abi;

use super::{emit_load_runtime_stub, emit_load_u64};
use crate::{
    artifact::relocation::RelocationCapture,
    entry::{NATIVE_FRAME_PC_OFFSET, TransitionTable},
};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_exception_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
    opcode: u8,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    side_exit: DynamicLabel,
    returned: DynamicLabel,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) {
    let resume = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    emit_load_u64(ops, 6, u64::from(opcode));
    emit_load_u64(ops, 2, arg0);
    emit_load_u64(ops, 1, arg1);
    emit_load_u64(ops, 8, arg2);
    emit_load_runtime_stub(
        ops,
        relocations,
        transitions.variadic_entry(abi::STUB_JIT_EXCEPTION_OP),
        abi::STUB_JIT_EXCEPTION_OP,
    );
    dynasm!(ops
        ; .arch x64
        ; call r11
        ; cmp edx, abi::NativeResultStatus::Continue as i32
        ; je =>done
        ; cmp edx, abi::NativeResultStatus::Success as i32
        ; je =>returned
        ; cmp edx, abi::NativeResultStatus::SideExit as i32
        ; je =>resume
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; =>resume
        ; mov [r14 + NATIVE_FRAME_PC_OFFSET as i32], eax
        ; jmp =>side_exit
        ; =>done
    );
}
