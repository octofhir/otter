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

use crate::{Frame, InterruptFlag, VmError};

pub(crate) fn register_operand(operand: Option<&Operand>) -> Result<u16, VmError> {
    match operand {
        Some(Operand::Register(r)) => Ok(*r),
        _ => Err(VmError::InvalidOperand),
    }
}

pub(crate) fn const_operand(operand: Option<&Operand>) -> Result<u32, VmError> {
    match operand {
        Some(Operand::ConstIndex(k)) => Ok(*k),
        _ => Err(VmError::InvalidOperand),
    }
}

pub(crate) fn imm32_operand(operand: Option<&Operand>) -> Result<i32, VmError> {
    match operand {
        Some(Operand::Imm32(v)) => Ok(*v),
        _ => Err(VmError::InvalidOperand),
    }
}

/// Apply a relative branch. Negative offsets are back-edges and poll the
/// interrupt flag.
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
