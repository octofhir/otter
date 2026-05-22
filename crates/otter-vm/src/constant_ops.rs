//! Constant-pool opcode helpers.
//!
//! Literal loads that require non-trivial decoding live here so dense dispatch
//! can keep using typed executable operands without keeping conversion logic in
//! `lib.rs`.
//!
//! # Contents
//! - BigInt literal materialisation.
//!
//! # Invariants
//! - Constant indexes are already decoded from executable operands.
//! - Helpers advance the current frame PC exactly once on success.
//!
//! # See also
//! - [`crate::execution_context::ExecutionContext`]
//! - [`crate::bigint`]

use crate::{ExecutionContext, Frame, Interpreter, Value, VmError, bigint, write_register};

impl Interpreter {
    pub(crate) fn run_load_bigint_reg(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        idx: u32,
    ) -> Result<(), VmError> {
        let decimal = context
            .bigint_decimal_constant(idx)
            .ok_or(VmError::InvalidOperand)?;
        let value = bigint::BigIntValue::from_decimal(&mut self.gc_heap, decimal)
            .ok_or(VmError::InvalidOperand)?
            .map_err(crate::oom_to_vm)?;
        write_register(frame, dst, Value::big_int(value))?;
        frame.pc += 1;
        Ok(())
    }
}
