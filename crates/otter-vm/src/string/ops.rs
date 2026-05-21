//! String opcode helpers.
//!
//! The high-level `String(...)` constructor and statics live in
//! [`crate::string::dispatch`]. This module owns lower-level VM string opcodes
//! that operate directly on registers.
//!
//! # Contents
//! - Fixed-width register helpers for string indexing.
//! - `typeof` string materialisation.
//!
//! # Invariants
//! - Inputs are decoded from the executable instruction format before reaching
//!   these helpers.
//! - String indexing is UTF-16 code-unit based, matching ECMAScript strings.
//!
//! # See also
//! - [`crate::string`]
//! - [`crate::executable`]

use crate::{Frame, Interpreter, JsString, Value, VmError, read_register, write_register};

impl Interpreter {
    pub(crate) fn run_typeof_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let tag = read_register(frame, src)?.typeof_string_with_heap(&self.gc_heap);
        let s = JsString::from_str(tag, &self.gc_heap)?;
        write_register(frame, dst, Value::String(s))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_get_string_index_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        recv: u16,
        idx: u16,
    ) -> Result<(), VmError> {
        let recv_s = read_register(frame, recv)?
            .as_string()
            .ok_or(VmError::TypeMismatch)?
            .clone();
        let idx = match read_register(frame, idx)? {
            Value::Number(n) => match n.as_smi() {
                Some(v) if v >= 0 => v as u32,
                _ => recv_s.len(),
            },
            _ => return Err(VmError::TypeMismatch),
        };
        let result = match recv_s.char_code_at(idx) {
            Some(unit) => JsString::from_utf16_units(&[unit], &self.gc_heap)?,
            None => JsString::empty(&self.gc_heap)?,
        };
        write_register(frame, dst, Value::String(result))?;
        frame.pc += 1;
        Ok(())
    }
}
