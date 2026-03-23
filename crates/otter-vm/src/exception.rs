//! Exception-table metadata for the new VM.

use crate::bytecode::ProgramCounter;

/// Static metadata for a single exception handler range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExceptionHandler {
    try_start: ProgramCounter,
    try_end: ProgramCounter,
    handler_pc: ProgramCounter,
}

impl ExceptionHandler {
    /// Creates an exception handler entry.
    #[must_use]
    pub const fn new(
        try_start: ProgramCounter,
        try_end: ProgramCounter,
        handler_pc: ProgramCounter,
    ) -> Self {
        Self {
            try_start,
            try_end,
            handler_pc,
        }
    }

    /// Returns the start of the protected range.
    #[must_use]
    pub const fn try_start(self) -> ProgramCounter {
        self.try_start
    }

    /// Returns the exclusive end of the protected range.
    #[must_use]
    pub const fn try_end(self) -> ProgramCounter {
        self.try_end
    }

    /// Returns the handler program counter.
    #[must_use]
    pub const fn handler_pc(self) -> ProgramCounter {
        self.handler_pc
    }
}

/// Immutable exception table for a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionTable {
    handlers: Box<[ExceptionHandler]>,
}

impl ExceptionTable {
    /// Creates an exception table from owned handler metadata.
    #[must_use]
    pub fn new(handlers: Vec<ExceptionHandler>) -> Self {
        Self {
            handlers: handlers.into_boxed_slice(),
        }
    }

    /// Creates an empty exception table.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Returns the number of handlers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    /// Returns `true` when the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Returns the immutable handler slice.
    #[must_use]
    pub fn handlers(&self) -> &[ExceptionHandler] {
        &self.handlers
    }
}

impl Default for ExceptionTable {
    fn default() -> Self {
        Self::empty()
    }
}
