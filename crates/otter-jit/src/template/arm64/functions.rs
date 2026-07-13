//! Function-construction transition emission.
//!
//! # Contents
//! - Reentrant call to the VM-owned `Function.prototype.bind` completion helper.
//! - Uniform success, throw, and exact pre-effect bailout routing.
//!
//! # Invariants
//! - The VM helper commits `Op::BindFunction` before returning success, so
//!   generated code only falls through once.
//! - A missing published activation is the sole bailout case and occurs before
//!   any observable `name`/`length` getter or bound-function allocation.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_bind_function`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::values::emit_load_u64;
use crate::entry::{STATUS_BAILED, STATUS_THREW};

/// Emit `Op::BindFunction`. `packed_meta` is `dst | callee<<16 | this<<32 |
/// argc<<48`; `packed_args` holds the bound-argument registers, one per lane.
pub(super) fn emit_bind_function(
    ops: &mut Assembler,
    transitions: &crate::entry::TransitionTable,
    packed_meta: u64,
    packed_args: u64,
    bail: DynamicLabel,
    threw: DynamicLabel,
) {
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_u64(ops, 1, packed_meta);
    emit_load_u64(ops, 2, packed_args);
    emit_load_u64(
        ops,
        16,
        transitions.variadic_entry(abi::STUB_JIT_BIND_FUNCTION),
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
