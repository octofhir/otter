//! The structural index: every position the scanner may have to stop at.
//!
//! # Contents
//! - [`Index`] — the positions, with a forward-only cursor over them.
//! - [`build`] — the index of a document, and the encoding check that runs in
//!   the same walk.
//! - [`bytes`] / [`utf16`] — the two producers, one per width of code unit,
//!   which the encodings choose between.
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
//! - Both producers work a block at a time — 64 bytes, or 64 code units —
//!   and never look at one on its own except to say where a block the kernel
//!   already rejected went wrong.
//! - Surrogate pairing is decided from the block's own bit sets: every
//!   leading surrogate owes the unit after it, so one shift and one
//!   comparison answer for a whole block, with a single bit carried between
//!   blocks.
//!
//! # See also
//! - [`crate::scan`] — the only consumer.
//! - [`crate::simd`] — the classification kernels.

use crate::chars::is_char;
use crate::encoding::{Encoding, Utf8};
use crate::error::{Error, ErrorKind, Result};
use crate::simd::{self, BLOCK, Kernels, PRECEDING, UNIT_BLOCK};

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
    bytes_with(document, validate_utf8, out, simd::current())
}

/// [`bytes`], with the kernels named explicitly.
///
/// Tests use this to run the portable kernels against the ones this machine
/// would otherwise pick.
///
/// # Errors
/// As [`build`].
pub fn bytes_with(
    document: &[u8],
    validate_utf8: bool,
    out: &mut Vec<u32>,
    kernels: Kernels,
) -> Result<()> {
    let Kernels {
        classify, validate, ..
    } = kernels;
    let mut padded = [b' '; BLOCK];
    // The bytes before the block being checked, so a sequence that straddles
    // the boundary is still whole. They are read out of the document itself,
    // which costs nothing per block; before the first block they are zeros,
    // which are ASCII and so leave nothing owed.
    let opening = [0u8; PRECEDING];
    let mut base = 0;
    // Whether anything above ASCII has been seen, which is the only reason
    // the walk has to be closed off at the end.
    let mut any_nonascii = false;
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
        // The vector check answers for the whole block at once, so legal text
        // in any script costs no decoding at all. Only a block it rejects is
        // walked, and only to say where and why.
        if validate_utf8 && (masks.nonascii != 0 || any_nonascii) {
            let preceding: &[u8; PRECEDING] = if base >= PRECEDING {
                document[base - PRECEDING..base]
                    .try_into()
                    .expect("a fixed-width window is exactly that wide")
            } else {
                &opening
            };
            if validate(preceding, block) != 0 {
                // The decoder says where and why. It starts from the top of
                // the document because a sequence may have begun in an
                // earlier block, and this is the failing path, walked once.
                check_utf8(document, 0, base + take)?;
            }
            any_nonascii = masks.nonascii != 0;
        }
        base += take;
    }
    // A sequence the document ends inside owes bytes that never came. One
    // block of spaces past the end demands them; what precedes that block is
    // the tail of the last one, padding included.
    if validate_utf8 && any_nonascii {
        let mut tail = [b' '; PRECEDING];
        let last = document.len().next_multiple_of(BLOCK);
        if last == document.len() {
            tail.copy_from_slice(&document[document.len() - PRECEDING..]);
        } else {
            tail.copy_from_slice(&padded[BLOCK - PRECEDING..]);
        }
        if validate(&tail, &[b' '; BLOCK]) != 0 {
            check_utf8(document, 0, document.len())?;
        }
    }
    Ok(())
}

/// Check that `document[from..]` spells legal characters at least as far as
/// `to`, and report how far that took. This is the slow path: the vector
/// check sends the walk here only for a block it has already rejected.
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

/// Index a UTF-16 document, classifying a block of code units at a time.
///
/// # Errors
/// Returns [`ErrorKind::IllegalCharacter`] for a unit outside the `Char`
/// production and [`ErrorKind::MalformedEncoding`] for a surrogate without
/// its partner.
pub fn utf16(document: &[u16], out: &mut Vec<u32>) -> Result<()> {
    utf16_with(document, out, simd::current())
}

/// [`utf16`], with the kernels named explicitly.
///
/// Tests use this to run the portable kernels against the ones this machine
/// would otherwise pick.
///
/// # Errors
/// As [`utf16`].
pub fn utf16_with(document: &[u16], out: &mut Vec<u32>, kernels: Kernels) -> Result<()> {
    let classify_units = kernels.classify_units;
    let mut padded = [simd::utf16::PAD; UNIT_BLOCK];
    let mut base = 0;
    // Whether the unit before this block was a leading surrogate, and so owes
    // a trailing one to the block's first unit.
    let mut owed = false;
    while base < document.len() {
        let take = (document.len() - base).min(UNIT_BLOCK);
        let block: &[u16; UNIT_BLOCK] = if take == UNIT_BLOCK {
            document[base..base + UNIT_BLOCK]
                .try_into()
                .expect("a full block is exactly one block wide")
        } else {
            padded[..take].copy_from_slice(&document[base..base + take]);
            // A space classifies as nothing, so the padding cannot be
            // mistaken for content — and a leading surrogate at the end of
            // the document is left owing a partner, which is the error it is.
            padded[take..].fill(simd::utf16::PAD);
            &padded
        };

        let masks = (classify_units)(block);
        // Every leading surrogate owes the unit after it, and every trailing
        // one is owed by the unit before it. Shifting one mask by a lane says
        // where the debts fall; the other says where they were paid.
        let owes = (masks.high << 1) | u64::from(owed);
        let unpaired = masks.low ^ owes;
        if masks.forbidden != 0 || unpaired != 0 {
            return Err(first_unit_error(
                document,
                base,
                masks.forbidden,
                unpaired,
                owes,
            ));
        }
        let mut structural = masks.structural;
        while structural != 0 {
            out.push((base + structural.trailing_zeros() as usize) as u32);
            structural &= structural - 1;
        }
        owed = masks.high >> (UNIT_BLOCK - 1) != 0;
        base += take;
    }
    // A leading surrogate in the document's last unit is owed a partner that
    // never came. A short tail is padded, so only a document that fills its
    // last block can reach here still owing one.
    if owed {
        return Err(Error::new(ErrorKind::MalformedEncoding, document.len() - 1));
    }
    Ok(())
}

/// Report whichever of the two problems in a block comes first, the way a
/// walk over the units one at a time would have found it.
fn first_unit_error(
    document: &[u16],
    base: usize,
    forbidden: u64,
    unpaired: u64,
    owes: u64,
) -> Error {
    let forbidden_at = if forbidden == 0 {
        usize::MAX
    } else {
        forbidden.trailing_zeros() as usize
    };
    let unpaired_at = if unpaired == 0 {
        usize::MAX
    } else {
        unpaired.trailing_zeros() as usize
    };
    if forbidden_at <= unpaired_at {
        let at = base + forbidden_at;
        return Error::new(ErrorKind::IllegalCharacter(u32::from(document[at])), at);
    }
    // A debt that went unpaid is reported against the surrogate that owed it,
    // one unit earlier; a payment nobody owed is reported where it lies.
    let owed_here = owes & (1u64 << unpaired_at) != 0;
    let at = base + unpaired_at - usize::from(owed_here);
    Error::new(ErrorKind::MalformedEncoding, at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chars::is_char;
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

    /// The verdict a walk over the units one at a time would reach: the
    /// definition the block walk has to match.
    fn utf16_by_decoding(document: &[u16]) -> Result<Vec<u32>> {
        let mut positions = Vec::new();
        let mut pos = 0;
        while pos < document.len() {
            let value = u32::from(document[pos]);
            if value < 0x80 {
                if crate::simd::utf16::unit_is_forbidden(document[pos]) {
                    return Err(Error::new(ErrorKind::IllegalCharacter(value), pos));
                }
                if crate::simd::utf16::unit_is_structural(document[pos]) {
                    positions.push(pos as u32);
                }
                pos += 1;
                continue;
            }
            let (code, width) = Utf16::decode(document, pos)?;
            if !is_char(code) {
                return Err(Error::new(ErrorKind::IllegalCharacter(code), pos));
            }
            pos += width;
        }
        Ok(positions)
    }

    fn assert_utf16_matches_the_decoder(text: &str, note: &str) {
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut positions = Vec::new();
        let block = utf16(&units, &mut positions).map(|()| positions);
        assert_eq!(
            block.as_ref().map_err(|err| (err.kind.clone(), err.offset)),
            utf16_by_decoding(&units)
                .as_ref()
                .map_err(|err| (err.kind.clone(), err.offset)),
            "{note}"
        );
    }

    #[test]
    fn a_utf16_document_is_indexed_as_the_decoder_would_index_it() {
        for text in [
            "<a b='1'>text</a>",
            "<a>日本語 &amp; more</a>\r\n",
            "<a>😀🚀 pairs across &lt;</a>",
            "<a/>",
            "",
        ] {
            assert_utf16_matches_the_decoder(text, text);
            // …and at every offset around a block boundary.
            for pad in 60..70 {
                let padded = format!("{}{text}", "x".repeat(pad));
                assert_utf16_matches_the_decoder(&padded, &format!("{text} at {pad}"));
            }
        }
    }

    #[test]
    fn a_surrogate_without_its_partner_is_caught_wherever_it_sits() {
        for pad in 60..70usize {
            for (units, at) in [
                (vec![0xD800u16], 0usize),
                (vec![0xDC00], 0),
                (vec![0xD800, u16::from(b'x')], 0),
                (vec![u16::from(b'x'), 0xDC00], 1),
                (vec![0xD800, 0xD800, 0xDC00], 0),
            ] {
                let mut document: Vec<u16> = "x".repeat(pad).encode_utf16().collect();
                let offset = document.len() + at;
                document.extend_from_slice(&units);
                let err = utf16(&document, &mut Vec::new()).unwrap_err();
                assert_eq!(err.kind, ErrorKind::MalformedEncoding, "pad {pad}");
                assert_eq!(err.offset, offset, "pad {pad}: {units:04X?}");
            }
        }
    }

    #[test]
    fn a_pair_split_across_a_block_boundary_is_whole() {
        for pad in 60..70usize {
            let mut document: Vec<u16> = "x".repeat(pad).encode_utf16().collect();
            document.extend("😀<a/>".encode_utf16());
            let mut positions = Vec::new();
            utf16(&document, &mut positions).unwrap();
            assert_eq!(
                positions,
                vec![(pad + 2) as u32, (pad + 5) as u32],
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
