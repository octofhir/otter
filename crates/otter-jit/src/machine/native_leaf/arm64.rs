//! Int32 replacements for VM-declared static-native leaves.
//!
//! # Contents
//! - [`emit_int32`] emits abs/max/min after the shared bootstrap identity guard.
//!
//! # Invariants
//! - Inputs occupy w1/w2 and the result w0, as selected before allocation.
//! - Int32 inputs cannot encode negative zero or NaN. Abs of INT32_MIN exits
//!   before committing a result so canonical Number completion stays exact.
//! - This module performs no calls, allocations, or JavaScript reentry.

use crate::Unsupported;
use dynasmrt::{DynamicLabel, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::native_abi::{
    RuntimeStubId, STUB_MATH_ABS_LEAF, STUB_MATH_MAX_LEAF, STUB_MATH_MIN_LEAF,
};

pub(crate) fn emit_int32(
    ops: &mut Assembler,
    stub: RuntimeStubId,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    if stub == STUB_MATH_ABS_LEAF.id {
        dynasm!(ops ; .arch aarch64
            ; cmp w1, #0
            ; cneg w0, w1, lt
            ; tbnz w0, #31, =>miss
        );
    } else if stub == STUB_MATH_MAX_LEAF.id {
        dynasm!(ops ; .arch aarch64 ; cmp w1, w2 ; csel w0, w1, w2, gt);
    } else if stub == STUB_MATH_MIN_LEAF.id {
        dynasm!(ops ; .arch aarch64 ; cmp w1, w2 ; csel w0, w1, w2, lt);
    } else {
        return Err(Unsupported::OperandShape("static-native Int32 declaration"));
    }
    Ok(())
}
