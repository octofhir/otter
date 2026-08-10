//! The structural index: every position the scanner may have to stop at.
//!
//! # Contents
//! - [`Index`] — the positions, with a forward-only cursor over them.
//! - [`build`] — the scalar producer, which validates the document's encoding
//!   in the same walk.
//!
//! # Invariants
//! - Indexed positions are exactly `<`, `>`, `&` and `\r`. Everything else a
//!   scanner needs to stop at is either inside a tag — scanned from the tag's
//!   own start — or a delimiter it is already looking for (`]]>`, `-->`, `?>`).
//! - Characters the `Char` production forbids never reach the index: they are
//!   rejected here, so a later stage never has to re-check content it skipped.
//! - Positions are ascending, and the cursor only ever moves forward.
//! - A run of text with no entry between its ends therefore contains no
//!   reference, no line-end to normalize and no `]]>`, so it can be handed on
//!   as a slice of the input without being examined again.
//!
//! # See also
//! - [`crate::scan`] — the only consumer.

use crate::chars::is_char;
use crate::encoding::{Encoding, Unit};
use crate::error::{Error, ErrorKind, Result};

/// ASCII dispositions: ordinary, worth indexing, or not allowed at all.
const ORDINARY: u8 = 0;
const STRUCTURAL: u8 = 1;
const FORBIDDEN: u8 = 2;

/// One entry per ASCII code point, so the hot loop is a single table read.
const ASCII_CLASS: [u8; 128] = {
    let mut table = [ORDINARY; 128];
    let mut code = 0usize;
    while code < 0x20 {
        table[code] = FORBIDDEN;
        code += 1;
    }
    table[0x09] = ORDINARY;
    table[0x0A] = ORDINARY;
    table[0x0D] = STRUCTURAL;
    table[b'<' as usize] = STRUCTURAL;
    table[b'>' as usize] = STRUCTURAL;
    table[b'&' as usize] = STRUCTURAL;
    table
};

/// The positions a scanner may have to stop at, in ascending order.
#[derive(Debug, Default)]
pub struct Index {
    positions: Vec<u32>,
}

impl Index {
    /// The first entry at or after `pos`, advancing `cursor` to it. Returns
    /// `None` once the entries are exhausted.
    #[inline]
    pub fn seek(&self, cursor: &mut usize, pos: usize) -> Option<usize> {
        while let Some(&entry) = self.positions.get(*cursor) {
            if entry as usize >= pos {
                return Some(entry as usize);
            }
            *cursor += 1;
        }
        None
    }

    /// How many entries the index holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.positions.len()
    }

    /// Whether the document had no structural character at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }
}

/// Index `units`, rejecting anything the document may not contain.
///
/// # Errors
/// Returns [`ErrorKind::IllegalCharacter`] for a code point outside the `Char`
/// production and [`ErrorKind::MalformedEncoding`] for units that do not spell
/// one in `E`.
pub fn build<E: Encoding>(units: &[E::Unit]) -> Result<Index> {
    let mut positions = Vec::with_capacity(units.len() / 16 + 8);
    let mut pos = 0usize;
    while pos < units.len() {
        let value = units[pos].value();
        if value < 0x80 {
            match ASCII_CLASS[value as usize] {
                ORDINARY => {}
                STRUCTURAL => positions.push(pos as u32),
                _ => return Err(Error::new(ErrorKind::IllegalCharacter(value), pos)),
            }
            pos += 1;
            continue;
        }
        // Non-ASCII is never structural, but it still has to spell a character
        // the document is allowed to contain.
        let (code, width) = E::decode(units, pos)?;
        if !is_char(code) {
            return Err(Error::new(ErrorKind::IllegalCharacter(code), pos));
        }
        pos += width;
    }
    Ok(Index { positions })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{Latin1, Utf8, Utf16};

    fn positions_utf8(text: &str) -> Vec<u32> {
        build::<Utf8>(text.as_bytes()).unwrap().positions
    }

    #[test]
    fn indexes_exactly_the_structural_characters() {
        let text = "a<b>c&d\re\tf\ng]]>h";
        let expected: Vec<u32> = text
            .bytes()
            .enumerate()
            .filter(|(_, byte)| matches!(byte, b'<' | b'>' | b'&' | b'\r'))
            .map(|(at, _)| at as u32)
            .collect();
        assert_eq!(positions_utf8(text), expected);
    }

    #[test]
    fn tag_internals_are_not_indexed() {
        // Quotes, `=` and the whitespace inside a tag stay out of the index;
        // only the tag's own delimiters are entries.
        let text = "<x y=\"1\tz\" w='2'>";
        assert_eq!(positions_utf8(text), vec![0, 16]);
    }

    #[test]
    fn forbidden_controls_are_rejected_wherever_they_appear() {
        for code in (0x00u8..0x20).filter(|c| !matches!(c, 0x09 | 0x0A | 0x0D)) {
            let doc = format!("<a>{}</a>", code as char);
            let err = build::<Utf8>(doc.as_bytes()).unwrap_err();
            assert_eq!(err.kind, ErrorKind::IllegalCharacter(u32::from(code)));
            assert_eq!(err.offset, 3);
        }
    }

    #[test]
    fn noncharacters_are_rejected_in_every_encoding() {
        let err = build::<Utf8>("<a>\u{FFFE}</a>".as_bytes()).unwrap_err();
        assert_eq!(err.kind, ErrorKind::IllegalCharacter(0xFFFE));
        let units: Vec<u16> = "<a>\u{FFFF}</a>".encode_utf16().collect();
        assert_eq!(
            build::<Utf16>(&units).unwrap_err().kind,
            ErrorKind::IllegalCharacter(0xFFFF)
        );
        // Latin-1 has no way to spell one, and its C1 range is legal content.
        assert!(build::<Latin1>(&[b'<', b'a', b'>', 0x85, b'<']).is_ok());
    }

    #[test]
    fn malformed_encoding_is_caught_while_indexing() {
        assert_eq!(
            build::<Utf8>(b"<a>\xC0\xAF</a>").unwrap_err().kind,
            ErrorKind::MalformedEncoding
        );
        assert_eq!(
            build::<Utf16>(&[b'<' as u16, 0xD800, b'>' as u16])
                .unwrap_err()
                .kind,
            ErrorKind::MalformedEncoding
        );
    }

    #[test]
    fn the_cursor_only_moves_forward() {
        let index = build::<Utf8>(b"a<b>c&d").unwrap();
        let mut cursor = 0;
        assert_eq!(index.seek(&mut cursor, 0), Some(1));
        assert_eq!(index.seek(&mut cursor, 2), Some(3));
        assert_eq!(index.seek(&mut cursor, 4), Some(5));
        assert_eq!(index.seek(&mut cursor, 6), None);
        assert_eq!(index.len(), 3);
    }

    #[test]
    fn clean_runs_are_recognised_without_looking_at_them() {
        // The first entry after the text starts is the `<` that ends it, so
        // the run in between needs no examination at all.
        let index = build::<Utf8>(b"<a>plain text</a>").unwrap();
        let mut cursor = 0;
        assert_eq!(index.seek(&mut cursor, 3), Some(13));
        // A run holding a reference stops earlier, at the `&`.
        let index = build::<Utf8>(b"<a>has &amp; ref</a>").unwrap();
        let mut cursor = 0;
        assert_eq!(index.seek(&mut cursor, 3), Some(7));
    }
}
