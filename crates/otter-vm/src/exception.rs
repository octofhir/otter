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

    /// Resolves the innermost handler covering the given program counter.
    #[must_use]
    pub fn find_handler(&self, pc: ProgramCounter) -> Option<ExceptionHandler> {
        let mut selected: Option<ExceptionHandler> = None;
        let mut selected_span = u32::MAX;

        for handler in self.handlers.iter().copied() {
            if !(handler.try_start() <= pc && pc < handler.try_end()) {
                continue;
            }

            let span = handler.try_end().saturating_sub(handler.try_start());
            let replace = match selected {
                None => true,
                Some(current) => {
                    span < selected_span
                        || (span == selected_span && handler.try_start() >= current.try_start())
                }
            };

            if replace {
                selected = Some(handler);
                selected_span = span;
            }
        }

        selected
    }
}

impl Default for ExceptionTable {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{ExceptionHandler, ExceptionTable};

    #[test]
    fn exception_table_prefers_innermost_covering_handler() {
        let outer = ExceptionHandler::new(0, 10, 100);
        let inner = ExceptionHandler::new(2, 5, 200);
        let table = ExceptionTable::new(vec![inner, outer]);

        assert_eq!(table.find_handler(1), Some(outer));
        assert_eq!(table.find_handler(3), Some(inner));
        assert_eq!(table.find_handler(9), Some(outer));
        assert_eq!(table.find_handler(10), None);
    }
}
