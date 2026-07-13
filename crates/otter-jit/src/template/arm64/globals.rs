//! Global-variable access transition emission.
//!
//! # Contents
//! - Reentrant calls to the VM-owned global environment-record helper.
//! - Uniform success, throw, and exact pre-effect bailout routing.
//!
//! # Invariants
//! - The VM helper commits every supported global opcode before returning
//!   success, so generated code only falls through once.
//! - A missing published activation is the sole bailout case and occurs before
//!   any observable accessor getter/setter or global-record mutation.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_global_op`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::values::emit_load_u64;
use crate::entry::{STATUS_BAILED, STATUS_THREW};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_global_op(
    ops: &mut Assembler,
    transitions: &crate::entry::TransitionTable,
    opcode: u8,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    bail: DynamicLabel,
    threw: DynamicLabel,
) {
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_u64(ops, 1, u64::from(opcode));
    emit_load_u64(ops, 2, arg0);
    emit_load_u64(ops, 3, arg1);
    emit_load_u64(ops, 4, arg2);
    emit_load_u64(ops, 16, transitions.variadic_entry(abi::STUB_JIT_GLOBAL_OP));
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; cbz x0, =>done
        ; cmp x0, STATUS_BAILED as u32
        ; b.eq =>bail
        ; cmp x0, STATUS_THREW as u32
        ; b.eq =>threw
        ; b =>threw
        ; =>done
    );
}
