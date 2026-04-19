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

    /// Looks up the source location active at the given program counter.
    ///
    /// Returns the entry with the largest `pc` that is `<= pc`. Entries must
    /// be sorted by program counter for this to be correct.
    #[must_use]
    pub fn lookup(&self, pc: ProgramCounter) -> Option<SourceLocation> {
        if self.entries.is_empty() {
            return None;
        }
        // Binary search for the largest entry with entry.pc <= pc.
        let mut lo = 0usize;
        let mut hi = self.entries.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.entries[mid].pc() <= pc {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            // No entry with pc <= input pc — fall back to the first entry,
            // which is the function's prologue location.
            Some(self.entries[0].location())
        } else {
            Some(self.entries[lo - 1].location())
        }
    }
}

impl Default for SourceMap {
    fn default() -> Self {
        Self::empty()
    }
}

// ============================================================
// D2: SourceTextIndex — byte offset → (line, column) mapping
// ============================================================

/// Lightweight index over a source-text buffer that resolves byte
/// offsets to `(line, column)` pairs. Pre-computes the byte
/// offset of every `\n` so each lookup is `O(log N)` in the
/// number of lines — amortised `O(1)` for the common case of
/// sequential statement emission.
///
/// Used by the source compiler to populate per-function
/// [`SourceMap`]s from oxc [`Span`] start offsets.
#[derive(Debug, Clone)]
pub struct SourceTextIndex {
    /// Byte offsets of every `\n` character in the source. Does
    /// not include a synthetic leading 0; line 1 starts at byte
    /// 0 implicitly.
    line_starts: Vec<u32>,
}

impl SourceTextIndex {
    /// Builds an index over `source` by scanning once for `\n`.
    /// Works for `\n` and `\r\n` line endings (the `\r` preceding
    /// the `\n` stays on the previous line, matching every
    /// mainstream JS tooling).
    #[must_use]
    pub fn new(source: &str) -> Self {
        let mut line_starts: Vec<u32> = vec![0];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                // Next line starts immediately after the `\n`.
                line_starts.push((i as u32).saturating_add(1));
            }
        }
        Self { line_starts }
    }

    /// Resolves a byte offset to a 1-based `(line, column)` pair.
    /// Offsets past the end clamp to the last line.
    #[must_use]
    pub fn resolve(&self, byte_offset: u32) -> SourceLocation {
        // Binary-search for the largest line-start ≤ offset.
        let idx = match self.line_starts.binary_search(&byte_offset) {
            Ok(i) => i,
            // binary_search returns the insertion point; the line
            // containing the offset is one before that.
            Err(0) => 0,
            Err(i) => i - 1,
        };
        let line = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let line_start = self.line_starts[idx];
        // 1-based column.
        let column = byte_offset.saturating_sub(line_start).saturating_add(1);
        SourceLocation::new(line, column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_text_index_resolves_line_and_column() {
        let source = "fn main() {\n  return 1;\n}\n";
        let idx = SourceTextIndex::new(source);
        // Offset 0 → line 1, col 1 ("f" of "fn").
        assert_eq!(idx.resolve(0), SourceLocation::new(1, 1));
        // Offset 12 → start of "  return 1;" → line 2, col 1.
        assert_eq!(idx.resolve(12), SourceLocation::new(2, 1));
        // Offset 14 → "return" → line 2, col 3.
        assert_eq!(idx.resolve(14), SourceLocation::new(2, 3));
        // Offset 24 → "}" → line 3, col 1.
        assert_eq!(idx.resolve(24), SourceLocation::new(3, 1));
    }

    #[test]
    fn source_text_index_handles_single_line_source() {
        let source = "let x = 42;";
        let idx = SourceTextIndex::new(source);
        assert_eq!(idx.resolve(0), SourceLocation::new(1, 1));
        assert_eq!(idx.resolve(4), SourceLocation::new(1, 5));
        // Past the end clamps to the last line.
        assert_eq!(idx.resolve(u32::MAX), SourceLocation::new(1, u32::MAX));
    }
}
