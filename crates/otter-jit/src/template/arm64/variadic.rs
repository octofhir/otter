//! Variadic construction transition emission.
//!
//! # Contents
//! - Reentrant calls to the VM-owned variadic helper.
//! - Uniform success, throw, and exact pre-effect bailout routing.
//!
//! # Invariants
//! - The VM helper commits every supported variadic opcode before returning
//!   success, so generated code only falls through once.
//! - A missing published activation is the sole bailout case.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_variadic_op`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::values::{emit_load_runtime_stub, emit_load_u64};
use crate::artifact::relocation::RelocationCapture;
use crate::entry::{STATUS_BAILED, STATUS_THREW};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_variadic_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    opcode: u8,
    prefix: u16,
    argc: u16,
    packed_args: u64,
    bail: DynamicLabel,
    threw: DynamicLabel,
) {
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_u64(ops, 1, u64::from(opcode));
    emit_load_u64(ops, 2, u64::from(prefix));
    emit_load_u64(ops, 3, u64::from(argc));
    emit_load_u64(ops, 4, packed_args);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.variadic_entry(abi::STUB_JIT_VARIADIC_OP),
        abi::STUB_JIT_VARIADIC_OP,
    );
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
