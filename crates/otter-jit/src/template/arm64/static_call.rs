//! Static intrinsic-call transition emission.
//!
//! # Contents
//! - Reentrant calls to the VM-owned static-call helper.
//! - Uniform success, throw, and exact pre-effect bailout routing.
//!
//! # Invariants
//! - The VM helper commits every supported static-call opcode before returning
//!   success, so generated code only falls through once.
//! - A missing published activation is the sole bailout case.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_static_call_op`

use dynasmrt::{DynamicLabel, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::values::{emit_load_runtime_stub, emit_load_u64};
use crate::artifact::relocation::RelocationCapture;

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_static_call_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    opcode: u8,
    packed_head: u64,
    method: u64,
    packed_args: u64,
    bail: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_u64(ops, 1, u64::from(opcode));
    emit_load_u64(ops, 2, packed_head);
    emit_load_u64(ops, 3, method);
    emit_load_u64(ops, 4, packed_args);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.variadic_entry(abi::STUB_JIT_STATIC_CALL_OP),
        abi::STUB_JIT_STATIC_CALL_OP,
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
    super::transitions::emit_status_word_result(ops, Some(bail), threw, fatal);
}
