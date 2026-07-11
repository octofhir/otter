//! Operand decoding helpers for VM dispatch.
//!
//! Keep small bytecode-decoding utilities out of the main value model so
//! `lib.rs` can stay focused on runtime state and dispatch behavior.
//!
//! # Contents
//! - Register, constant-index, and signed-immediate decoders.
//! - Relative branch application with interrupt polling on back-edges.
//!
//! # Invariants
//! - Decoder failures are structural bytecode errors and surface as
//!   [`VmError::InvalidOperand`].
//! - Negative branch offsets are back-edges and must poll the interrupt flag.
//!
//! # See also
//! - [`crate::executable`]

use otter_bytecode::Operand;
use std::borrow::Borrow;

use crate::{Frame, InterruptFlag, VmError};

pub(crate) fn register_operand<T: Borrow<Operand>>(operand: Option<T>) -> Result<u16, VmError> {
    match operand.as_ref().map(Borrow::borrow) {
        Some(Operand::Register(r)) => Ok(*r),
        _ => Err(VmError::InvalidOperand),
    }
}

pub(crate) fn const_operand<T: Borrow<Operand>>(operand: Option<T>) -> Result<u32, VmError> {
    match operand.as_ref().map(Borrow::borrow) {
        Some(Operand::ConstIndex(k)) => Ok(*k),
        _ => Err(VmError::InvalidOperand),
    }
}

/// Apply a relative branch. `offset` is a signed byte-offset delta
/// relative to `(frame.pc + 1)` — the byte right after the branch
/// opcode — matching the encoding produced by the executable builder.
/// Negative offsets are back-edges and poll the interrupt flag so a
/// long-running loop can be cancelled cooperatively.
pub(crate) fn apply_branch(
    frame: &mut Frame,
    offset: i32,
    interrupt: &InterruptFlag,
) -> Result<(), VmError> {
    let next_pc = (frame.pc as i64 + 1).saturating_add(offset as i64);
    if next_pc < 0 || next_pc > u32::MAX as i64 {
        return Err(VmError::InvalidOperand);
    }
    if offset < 0 && interrupt.is_set() {
        return Err(VmError::Interrupted);
    }
    frame.pc = next_pc as u32;
    Ok(())
}
