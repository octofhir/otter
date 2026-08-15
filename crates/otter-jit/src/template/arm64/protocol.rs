//! Committed object property-protocol value emission.
//!
//! # Contents
//! - Fixed boxed-value calls to the VM-owned property-protocol kernels.
//! - Normal-result commit and rooted JavaScript-throw routing.
//!
//! # Invariants
//! - The published function/PC selects semantics; no opcode, destination,
//!   register index, or materialized-frame identity crosses the ABI.
//! - Once semantic entry begins, the VM returns `Ok(value)` or
//!   `Throw(exception)`. Pre-entry `Fatal` bypasses local JS handlers and
//!   propagates the parked structural error. Proxy traps and `@@hasInstance`
//!   are never replayed.
//!
//! # See also
//! - `otter_vm::RuntimeCall::object_protocol_values`

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi as abi;

use super::values::{emit_load_reg, emit_load_runtime_stub, emit_load_u64, emit_store_reg};
use crate::artifact::relocation::RelocationCapture;
use crate::entry::{Unsupported, VALUE_UNDEFINED};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_object_protocol_value(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &crate::entry::TransitionTable,
    _operation: otter_vm::ObjectProtocolValueOp,
    result: Option<u16>,
    value0: u16,
    value1: Option<u16>,
    committed_throw: DynamicLabel,
    fatal: DynamicLabel,
) -> Result<(), Unsupported> {
    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
    emit_load_reg(ops, 1, value0)?;
    if let Some(value1) = value1 {
        emit_load_reg(ops, 2, value1)?;
    } else {
        emit_load_u64(ops, 2, VALUE_UNDEFINED);
    }
    emit_load_runtime_stub(
        ops,
        relocations,
        16,
        transitions.entry(abi::STUB_JIT_OBJECT_PROTOCOL_VALUE),
        abi::STUB_JIT_OBJECT_PROTOCOL_VALUE,
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; mov x15, x1
        ; cbz x15, >normal
        ; cmp x15, abi::NativeResultStatus::Throw as u32
        ; b.eq =>committed_throw
        ; b =>fatal
        ; normal:
    );
    if let Some(result) = result {
        emit_store_reg(ops, 0, result)?;
    }
    Ok(())
}
