//! Committed object-protocol emission with an explicit semantic operation.
//!
//! # Contents
//! - Boxed operands and the VM-owned operation word at the platform C boundary.
//! - Result commit, rooted JavaScript throws and terminal structural exits.
//!
//! # Invariants
//! - The producer chooses semantics; source/root records own GC and recovery.
//! - Operations complete once, including Proxy traps and `@@hasInstance`.
//! - The shared C boundary owns shadow space and hidden pair results.
//!
//! # See also
//! - `otter_vm::native_abi::RuntimeCall::object_protocol_values`
//! - `crate::x86_64::call_abi`

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_object_protocol_value(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    operation: abi::ObjectProtocolValueOp,
    result: Option<u16>,
    value0: u16,
    value1: Option<u16>,
    throw_value: DynamicLabel,
    fatal: DynamicLabel,
) {
    emit_load_reg(ops, 6, value0);
    match value1 {
        Some(register) => emit_load_reg(ops, 2, register),
        None => emit_load_u64(ops, 2, VALUE_UNDEFINED),
    }
    emit_load_u64(ops, 1, operation.encode_abi());
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    let descriptor = abi::STUB_JIT_OBJECT_PROTOCOL_VALUE;
    emit_load_runtime_stub(ops, relocations, transitions.entry(descriptor), descriptor);
    emit_runtime_call(ops, descriptor);
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; je >completed
        ; cmp edx, abi::NativeResultStatus::Throw as i32
        ; je =>throw_value
        ; jmp =>fatal
        ; completed:
    );
    if let Some(result) = result {
        emit_store_reg(ops, 0, result);
    }
}
