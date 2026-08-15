//! Class and dynamic-value transition emission.
//!
//! # Contents
//! - Uniform reentrant dispatch to the VM-owned class/value helper.
//! - Shared success, exact-bail, and throw routing.
//!
//! # Invariants
//! - Operand words carry only schema-decoded registers, constants, and flags.
//! - The VM commits the entire opcode before a success fallthrough.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_class_value_op`

use dynasmrt::{DynamicLabel, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::values::{emit_load_runtime_stub, emit_load_u64};
use crate::artifact::relocation::RelocationCapture;

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_class_value_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    opcode: u8,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    bail: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_u64(ops, 1, u64::from(opcode));
    emit_load_u64(ops, 2, arg0);
    emit_load_u64(ops, 3, arg1);
    emit_load_u64(ops, 4, arg2);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.variadic_entry(abi::STUB_JIT_CLASS_VALUE_OP),
        abi::STUB_JIT_CLASS_VALUE_OP,
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
    super::transitions::emit_status_word_result(ops, Some(bail), threw, fatal);
}
