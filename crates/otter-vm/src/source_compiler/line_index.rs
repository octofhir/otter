//! Fast byte-offset → `(line, column)` resolver for a single source text.
//!
//! This is the building block for populating `SourceMap` entries during
//! compilation. The compiler has the *generated JS* text in scope; for every
//! AST node it sees, we resolve the node's span's start byte to a 1-based
//! `(line, column)` pair via binary search over a precomputed line-start
//! table.
//!
//! The column numbers are **UTF-16 code units** (matching the V3 source-map
//! and JS/TS convention), not bytes. This keeps the mapping round-trippable
//! with `oxc_sourcemap::SourceMap::lookup_token`.

use crate::source_map::SourceLocation;

/// Precomputed line-start byte offsets for a single source text.
#[derive(Debug, Clone)]
pub struct SourceLineIndex {
    /// Byte offsets of the start of each line. The first entry is always `0`.
    /// A trailing entry equal to `source.len()` is implicit — lookups beyond
    /// the last recorded line clamp to the last line.
    line_starts: Box<[u32]>,
    /// The source text itself. Borrowed by lookups to count UTF-16 code units
    /// between a line start and the target byte offset.
    source: Box<str>,
}

impl SourceLineIndex {
    /// Builds a line index for the given source text.
    #[must_use]
    pub fn new(source: &str) -> Self {
        let mut line_starts = Vec::with_capacity(source.len() / 32 + 1);
        line_starts.push(0u32);
        let bytes = source.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            // Normalize CRLF / CR / LF. `oxc` spans use byte offsets into the
            // original text, so we keep the byte offsets verbatim but advance
            // one "line" per terminator.
            if b == b'\n' {
                if let Ok(next) = u32::try_from(i + 1) {
                    line_starts.push(next);
                }
                i += 1;
            } else if b == b'\r' {
                let skip = if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    2
                } else {
                    1
                };
                if let Ok(next) = u32::try_from(i + skip) {
                    line_starts.push(next);
                }
                i += skip;
            } else {
                i += 1;
            }
        }

        Self {
            line_starts: line_starts.into_boxed_slice(),
            source: source.to_string().into_boxed_str(),
        }
    }

    /// Returns the 1-based `(line, column)` for a byte offset into the source.
    ///
    /// Columns are counted in **UTF-16 code units** starting from 1, matching
    /// the V3 source-map convention. Offsets past the end of the source clamp
    /// to the final line's last column.
    #[must_use]
    pub fn locate(&self, byte_offset: u32) -> SourceLocation {
        let clamped = byte_offset.min(self.source.len() as u32);
        // Binary search: find the largest line_starts[i] <= clamped.
        let line_idx = match self.line_starts.binary_search(&clamped) {
            Ok(idx) => idx,
            Err(idx) => idx.saturating_sub(1),
        };
        let line_start = self.line_starts[line_idx];
        let column_units = utf16_len(&self.source[line_start as usize..clamped as usize]);
        // 1-based line and column.
        SourceLocation::new(
            u32::try_from(line_idx + 1).unwrap_or(u32::MAX),
            column_units.saturating_add(1),
        )
    }

    /// Returns the number of lines in the source.
    #[must_use]
    #[allow(dead_code)] // Exposed for future byte-range helpers used by diagnostic rendering.
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Returns the byte range for a 1-based line number. `None` if out of range.
    #[must_use]
    #[allow(dead_code)] // Exposed for future byte-range helpers used by diagnostic rendering.
    pub fn line_byte_range(&self, line_1based: u32) -> Option<(u32, u32)> {
        if line_1based == 0 {
            return None;
        }
        let idx = (line_1based - 1) as usize;
        let start = *self.line_starts.get(idx)?;
        let end = self
            .line_starts
            .get(idx + 1)
            .copied()
            .unwrap_or(self.source.len() as u32);
        Some((start, end))
    }
}

/// Counts the number of UTF-16 code units in a UTF-8 string slice.
fn utf16_len(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_source() {
        let idx = SourceLineIndex::new("");
        let loc = idx.locate(0);
        assert_eq!(loc.line(), 1);
        assert_eq!(loc.column(), 1);
    }

    #[test]
    fn single_line() {
        let idx = SourceLineIndex::new("abcdef");
        assert_eq!(idx.locate(0), SourceLocation::new(1, 1));
        assert_eq!(idx.locate(3), SourceLocation::new(1, 4));
        assert_eq!(idx.locate(6), SourceLocation::new(1, 7));
    }

    #[test]
    fn multi_line_lf() {
        // "a\nbb\nccc"
        //  ^ L1C1
        //     ^ L2C1
        //        ^ L3C1
        let idx = SourceLineIndex::new("a\nbb\nccc");
        assert_eq!(idx.locate(0), SourceLocation::new(1, 1));
        assert_eq!(idx.locate(1), SourceLocation::new(1, 2));
        assert_eq!(idx.locate(2), SourceLocation::new(2, 1));
        assert_eq!(idx.locate(4), SourceLocation::new(2, 3));
        assert_eq!(idx.locate(5), SourceLocation::new(3, 1));
        assert_eq!(idx.locate(8), SourceLocation::new(3, 4));
    }

    #[test]
    fn crlf() {
        // "a\r\nb" — byte 3 is 'b' on line 2 col 1.
        let idx = SourceLineIndex::new("a\r\nb");
        assert_eq!(idx.locate(0), SourceLocation::new(1, 1));
        assert_eq!(idx.locate(3), SourceLocation::new(2, 1));
    }

    #[test]
    fn cr_only() {
        let idx = SourceLineIndex::new("a\rb");
        assert_eq!(idx.locate(0), SourceLocation::new(1, 1));
        assert_eq!(idx.locate(2), SourceLocation::new(2, 1));
    }

    #[test]
    fn utf8_multibyte_columns_use_utf16_units() {
        // "αβ" — two 2-byte UTF-8 chars, each one UTF-16 code unit.
        // "α" is 2 bytes → locate(2) should give column 2 (UTF-16 unit count).
        let idx = SourceLineIndex::new("αβ");
        assert_eq!(idx.locate(0), SourceLocation::new(1, 1));
        assert_eq!(idx.locate(2), SourceLocation::new(1, 2));
        assert_eq!(idx.locate(4), SourceLocation::new(1, 3));
    }

    #[test]
    fn emoji_uses_surrogate_pair() {
        // "a😀b" — 😀 is 4 UTF-8 bytes, 2 UTF-16 code units (surrogate pair).
        // After 😀, column should advance by 2.
        let src = "a😀b";
        let idx = SourceLineIndex::new(src);
        assert_eq!(idx.locate(0), SourceLocation::new(1, 1));
        // "a" — col 2 before the emoji.
        assert_eq!(idx.locate(1), SourceLocation::new(1, 2));
        // After the 4-byte emoji, col = 1 + 1(a) + 2(emoji) = 4.
        assert_eq!(idx.locate(5), SourceLocation::new(1, 4));
    }

    #[test]
    fn past_end_clamps_to_last_line() {
        let idx = SourceLineIndex::new("abc");
        let loc = idx.locate(1000);
        assert_eq!(loc.line(), 1);
        assert_eq!(loc.column(), 4);
    }
}
