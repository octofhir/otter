//! Constant-pool opcode helpers.
//!
//! Literal loads that require non-trivial decoding live here so dense dispatch
//! can keep using typed executable operands without keeping conversion logic in
//! `lib.rs`.
//!
//! # Contents
//! - BigInt literal materialisation into the shared literal cells.
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
        let value = self.load_bigint_constant_value(context, idx)?;
        write_register(frame, dst, value)?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn load_bigint_constant_value(
        &mut self,
        context: &ExecutionContext,
        idx: u32,
    ) -> Result<Value, VmError> {
        let key = context.constant_cache_key(idx);
        if let Some(value) = self.literal_cells.get(&key) {
            if !value.is_big_int() {
                return Err(VmError::InvalidOperand);
            }
            return Ok(**value);
        }
        let decimal = context
            .bigint_decimal_constant(idx)
            .ok_or(VmError::InvalidOperand)?;
        let value = bigint::BigIntValue::from_decimal(&mut self.gc_heap, decimal)
            .ok_or(VmError::InvalidOperand)?
            .map_err(crate::oom_to_vm)?;
        let value = Value::big_int(value);
        let replaced = self.literal_cells.insert(key, Box::new(value));
        debug_assert!(replaced.is_none(), "literals canonicalize once");
        Ok(value)
    }
}
