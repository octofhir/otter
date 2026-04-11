//! `ExecutionResult` return type from `Interpreter::execute*`.

use crate::value::RegisterValue;

/// Successful execution result from the interpreter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExecutionResult {
    return_value: RegisterValue,
}

impl ExecutionResult {
    /// Creates a successful execution result.
    #[must_use]
    pub const fn new(return_value: RegisterValue) -> Self {
        Self { return_value }
    }

    /// Returns the raw return value.
    #[must_use]
    pub const fn return_value(self) -> RegisterValue {
        self.return_value
    }
}
