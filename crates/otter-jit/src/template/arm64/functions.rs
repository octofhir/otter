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

use dynasmrt::{DynamicLabel, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::values::{emit_load_runtime_stub, emit_load_u64};
use crate::artifact::relocation::RelocationCapture;

/// Emit `Op::BindFunction`. `packed_meta` is `dst | callee<<16 | this<<32 |
/// argc<<48`; `packed_args` holds the bound-argument registers, one per lane.
pub(super) fn emit_bind_function(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    packed_meta: u64,
    packed_args: u64,
    bail: DynamicLabel,
    threw: DynamicLabel,
    fatal: DynamicLabel,
) {
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_u64(ops, 1, packed_meta);
    emit_load_u64(ops, 2, packed_args);
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.variadic_entry(abi::STUB_JIT_BIND_FUNCTION),
        abi::STUB_JIT_BIND_FUNCTION,
    );
    dynasm!(ops ; .arch aarch64 ; blr x16);
    super::transitions::emit_status_word_result(ops, Some(bail), threw, fatal);
}
