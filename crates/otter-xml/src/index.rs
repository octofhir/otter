//! The structural index: every position the scanner may have to stop at.
//!
//! # Contents
//! - [`Index`] — the positions, with a forward-only cursor over them.
//! - [`build`] — the index of a document, and the encoding check that runs in
//!   the same walk.
//! - [`bytes`] / [`units`] — the byte and the portable producers, which the
//!   encodings choose between.
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
//! - The byte producer classifies 64 bytes at a time and only ever looks at a
//!   byte one at a time to check an encoding, which for a document that is
//!   mostly ASCII means almost never.
//!
//! # See also
//! - [`crate::scan`] — the only consumer.
//! - [`crate::simd`] — the classification kernels.

use crate::chars::is_char;
use crate::encoding::{Encoding, Unit, Utf8};
use crate::error::{Error, ErrorKind, Result};
use crate::simd::{self, BLOCK, Classifier};

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
    E::index_into(units, &mut positions)?;
    Ok(Index { positions })
}

/// Index a byte document, classifying a block at a time.
///
/// `validate_utf8` says whether a non-ASCII byte begins a multi-byte sequence
/// that has to be checked, or is a character in its own right.
///
/// # Errors
/// As [`build`].
pub fn bytes(document: &[u8], validate_utf8: bool, out: &mut Vec<u32>) -> Result<()> {
    bytes_with(document, validate_utf8, out, simd::classifier())
}

/// [`bytes`], with the classification kernel named explicitly.
///
/// Tests use this to run the portable classifier against the one this machine
/// would otherwise pick.
///
/// # Errors
/// As [`build`].
pub fn bytes_with(
    document: &[u8],
    validate_utf8: bool,
    out: &mut Vec<u32>,
    classify: Classifier,
) -> Result<()> {
    let mut padded = [b' '; BLOCK];
    let mut base = 0;
    // How far the encoding has been checked; a sequence may run past the end
    // of the block that started it.
    let mut checked = 0usize;
    while base < document.len() {
        let take = (document.len() - base).min(BLOCK);
        let block: &[u8; BLOCK] = if take == BLOCK {
            document[base..base + BLOCK]
                .try_into()
                .expect("a full block is exactly one block wide")
        } else {
            padded[..take].copy_from_slice(&document[base..base + take]);
            // A space classifies as nothing, so the padding cannot be mistaken
            // for content.
            padded[take..].fill(b' ');
            &padded
        };

        let masks = classify(block);
        if masks.forbidden != 0 {
            let at = base + masks.forbidden.trailing_zeros() as usize;
            return Err(Error::new(
                ErrorKind::IllegalCharacter(u32::from(document[at])),
                at,
            ));
        }
        let mut structural = masks.structural;
        while structural != 0 {
            out.push((base + structural.trailing_zeros() as usize) as u32);
            structural &= structural - 1;
        }
        if validate_utf8 && masks.nonascii != 0 {
            let first = base + masks.nonascii.trailing_zeros() as usize;
            checked = check_utf8(document, checked.max(first), base + take)?;
        }
        base += take;
    }
    Ok(())
}

/// Check that `document[from..]` spells legal characters at least as far as
/// `to`, and report how far that took.
fn check_utf8(document: &[u8], from: usize, to: usize) -> Result<usize> {
    let mut pos = from;
    while pos < to {
        if document[pos] < 0x80 {
            pos += 1;
            continue;
        }
        let (code, width) = Utf8::decode(document, pos)?;
        if !is_char(code) {
            return Err(Error::new(ErrorKind::IllegalCharacter(code), pos));
        }
        pos += width;
    }
    Ok(pos)
}

/// Index a document one code unit at a time, for encodings with no kernel.
///
/// # Errors
/// As [`build`].
pub fn units<E: Encoding>(document: &[E::Unit], out: &mut Vec<u32>) -> Result<()> {
    let mut pos = 0usize;
    while pos < document.len() {
        let value = document[pos].value();
        if value < 0x80 {
            let byte = value as u8;
            let class = simd::LUT_LO[(byte & 0x0F) as usize] & simd::LUT_HI[(byte >> 4) as usize];
            if class & simd::FORBIDDEN_BITS != 0 {
                return Err(Error::new(ErrorKind::IllegalCharacter(value), pos));
            }
            if class & simd::STRUCTURAL_BITS != 0 {
                out.push(pos as u32);
            }
            pos += 1;
            continue;
        }
        // Non-ASCII is never structural, but it still has to spell a character
        // the document is allowed to contain.
        let (code, width) = E::decode(document, pos)?;
        if !is_char(code) {
            return Err(Error::new(ErrorKind::IllegalCharacter(code), pos));
        }
        pos += width;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{Latin1, Utf16};

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
    fn a_sequence_straddling_a_block_boundary_is_still_checked() {
        for pad in 60..70usize {
            let mut doc = "x".repeat(pad).into_bytes();
            doc.extend_from_slice(b"\xEF\xBF\xBE<a/>"); // U+FFFE
            let err = build::<Utf8>(&doc).unwrap_err();
            assert_eq!(err.kind, ErrorKind::IllegalCharacter(0xFFFE), "pad {pad}");
            let mut doc = "x".repeat(pad).into_bytes();
            doc.extend_from_slice(b"\xE2\x82"); // truncated
            assert_eq!(
                build::<Utf8>(&doc).unwrap_err().kind,
                ErrorKind::MalformedEncoding,
                "pad {pad}"
            );
        }
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
