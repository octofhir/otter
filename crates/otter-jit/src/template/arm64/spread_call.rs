//! Spread/call-family transition emission.
//!
//! # Contents
//! - Reentrant calls to the VM-owned synchronous call/construct helper.
//! - Uniform success, throw, and committed caller-handler resumption routing.
//!
//! # Invariants
//! - Every success represents a fully committed opcode and falls through once.
//! - `STATUS_BAILED` carries either the sole pre-effect activation miss or a
//!   caller handler PC already published by the VM; neither path replays an
//!   observable call.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_spread_call_op`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::values::{emit_load_runtime_stub, emit_load_u64};
use crate::artifact::relocation::RelocationCapture;
use crate::entry::{STATUS_BAILED, STATUS_THREW};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_spread_call_op(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
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
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.variadic_entry(abi::STUB_JIT_SPREAD_CALL_OP),
        abi::STUB_JIT_SPREAD_CALL_OP,
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
