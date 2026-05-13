//! Error-object opcode helpers.
//!
//! Error constructors are fixed-width bytecodes and should stay on the compact
//! executable operand path instead of the fallback operand-slice path.
//!
//! # Contents
//! - `new Error(message)` object allocation.
//! - Native error constructor allocation (`TypeError`, `RangeError`, ...).
//! - Native error constructor loading for identifier reads.
//!
//! # Invariants
//! - Error kind names are compiler-emitted string constants.
//! - Allocated instances come from the interpreter's `ErrorClassRegistry` so
//!   prototype identity matches `instanceof`.
//!
//! # See also
//! - [`crate::error_classes`]
//! - [`crate::executable`]

use crate::{
    ErrorKind, ExecutionContext, Frame, Interpreter, Value, VmError, read_register, write_register,
};

impl Interpreter {
    pub(crate) fn run_new_error_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        msg_reg: u16,
    ) -> Result<(), VmError> {
        let value = read_register(frame, msg_reg)?.clone();
        let owned_message = error_message_from_value(value);
        let obj = {
            let string_heap = self.string_heap.clone();
            let registry = self.error_classes.clone();
            registry.make_instance(
                ErrorKind::Error,
                owned_message.as_deref(),
                &string_heap,
                &mut self.gc_heap,
            )?
        };
        write_register(frame, dst, Value::Object(obj))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_new_builtin_error_regs(
        &mut self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        kind_idx: u32,
        msg_reg: u16,
    ) -> Result<(), VmError> {
        let kind_name = context
            .string_constant_str(kind_idx)
            .ok_or(VmError::InvalidOperand)?;
        let kind = ErrorKind::from_class_name(kind_name).ok_or(VmError::InvalidOperand)?;
        let value = read_register(frame, msg_reg)?.clone();
        let owned_message = error_message_from_value(value);
        let obj = {
            let string_heap = self.string_heap.clone();
            let registry = self.error_classes.clone();
            registry.make_instance(
                kind,
                owned_message.as_deref(),
                &string_heap,
                &mut self.gc_heap,
            )?
        };
        write_register(frame, dst, Value::Object(obj))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_load_builtin_error_reg(
        &self,
        context: &ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        kind_idx: u32,
    ) -> Result<(), VmError> {
        let kind_name = context
            .string_constant_str(kind_idx)
            .ok_or(VmError::InvalidOperand)?;
        let kind = ErrorKind::from_class_name(kind_name).ok_or(VmError::InvalidOperand)?;
        let ctor = self.error_classes.constructor(kind);
        write_register(frame, dst, Value::Object(ctor))?;
        frame.pc += 1;
        Ok(())
    }
}

fn error_message_from_value(value: Value) -> Option<String> {
    match value {
        Value::Undefined => None,
        Value::String(s) => Some(s.to_lossy_string()),
        other => Some(other.display_string()),
    }
}
