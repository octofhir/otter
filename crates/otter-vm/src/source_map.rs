//! Source-location metadata for the new VM.

use crate::bytecode::ProgramCounter;

/// Source location in the original program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceLocation {
    line: u32,
    column: u32,
}

impl SourceLocation {
    /// Creates a source location.
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }

    /// Returns the 1-based line.
    #[must_use]
    pub const fn line(self) -> u32 {
        self.line
    }

    /// Returns the 1-based column.
    #[must_use]
    pub const fn column(self) -> u32 {
        self.column
    }
}

/// Single source-map entry keyed by program counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceMapEntry {
    pc: ProgramCounter,
    location: SourceLocation,
}

impl SourceMapEntry {
    /// Creates a source-map entry.
    #[must_use]
    pub const fn new(pc: ProgramCounter, location: SourceLocation) -> Self {
        Self { pc, location }
    }

    /// Returns the program counter.
    #[must_use]
    pub const fn pc(self) -> ProgramCounter {
        self.pc
    }

    /// Returns the source location.
    #[must_use]
    pub const fn location(self) -> SourceLocation {
        self.location
    }
}

/// Immutable source-map table for a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMap {
    entries: Box<[SourceMapEntry]>,
}

impl SourceMap {
    /// Creates a source map from owned entries.
    #[must_use]
    pub fn new(entries: Vec<SourceMapEntry>) -> Self {
        Self {
            entries: entries.into_boxed_slice(),
        }
    }

    /// Creates an empty source map.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Returns the number of source-map entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when the source map is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the immutable entry slice.
    #[must_use]
    pub fn entries(&self) -> &[SourceMapEntry] {
        &self.entries
    }
}

impl Default for SourceMap {
    fn default() -> Self {
        Self::empty()
    }
}
