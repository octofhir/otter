//! Vector UTF-8 checking: three nibble lookups per byte, no decoding.
//!
//! Checking a sequence by decoding it costs a branch per byte and is the
//! price most validating parsers pay for correctness. It is not necessary:
//! everything that makes a UTF-8 sequence illegal — a lead byte with the
//! wrong number of continuations, an overlong form, a surrogate, a code point
//! past U+10FFFF — is decided by the high nibble of the lead byte, its low
//! nibble, and the high nibble of the byte after it. Three table lookups per
//! byte answer all of it at once, and a vector does sixteen bytes per lookup.
//!
//! The tables are the ones from Keiser and Lemire, *Validating UTF-8 In Less
//! Than One Instruction Per Byte*. What is XML's own is the last check: the
//! `Char` production forbids U+FFFE and U+FFFF, which are well-formed UTF-8,
//! so the byte triple that spells them is matched in the same pass.
//!
//! # Contents
//! - [`Validator`] — a kernel that checks one block against its predecessor.
//! - [`validate_scalar`] — the portable one, and the definition the vector
//!   kernels answer to.
//! - [`LEAD_HIGH`] / [`LEAD_LOW`] / [`SECOND_HIGH`] — the shared tables.
//!
//! # Invariants
//! - A kernel reports a bit for every byte that takes part in something
//!   illegal, and zero for a block that is legal. It never reports zero for a
//!   block holding an error: the index trusts a clean answer and looks no
//!   further, and only a non-zero answer sends it to the decoder for the
//!   exact position and reason.
//! - A sequence may straddle a block boundary, so every kernel is handed the
//!   [`PRECEDING`] bytes before the block and shifts the last of them in.
//!   Before the first block those are zeros, which are ASCII and therefore
//!   start nothing. They are the widest vector's worth rather than three
//!   because a kernel shifts whole vectors, and taking them from the document
//!   itself means the walk copies nothing per block.
//! - A sequence may also run off the end of the document. The caller closes
//!   the walk with a block of spaces, so a lead byte whose continuations
//!   never arrive is reported like any other truncation.
//! - Every kernel returns what [`validate_scalar`] returns. That is a test,
//!   not a hope: `tests/index_agreement.rs` runs them over random input.
//!
//! # See also
//! - <https://arxiv.org/abs/2010.03090>
//! - [`crate::index`] — the only caller.

use super::BLOCK;

/// A lead byte whose continuations did not follow.
const TOO_SHORT: u8 = 1 << 0;
/// A continuation byte with no lead byte before it.
const TOO_LONG: u8 = 1 << 1;
/// A three-byte form spelling what two bytes could have said.
const OVERLONG_3: u8 = 1 << 2;
/// A code point past U+10FFFF.
const TOO_LARGE: u8 = 1 << 3;
/// A code point in the surrogate range, which UTF-8 may not encode.
const SURROGATE: u8 = 1 << 4;
/// A two-byte form spelling what one byte could have said.
const OVERLONG_2: u8 = 1 << 5;
/// The boundary case of [`TOO_LARGE`]: a lead of `0xF4` with a second byte
/// of `0x90` or above.
const TOO_LARGE_1000: u8 = 1 << 6;
/// A four-byte form spelling what three bytes could have said. Shares a bit
/// with [`TOO_LARGE_1000`]: no lead byte can be both.
const OVERLONG_4: u8 = 1 << 6;
/// Two continuation bytes in a row, which is legal only inside a three- or
/// four-byte sequence.
const TWO_CONTS: u8 = 1 << 7;

/// The errors a lead byte's low nibble cannot rule out on its own, and so
/// carries through to the other two tables.
const CARRY: u8 = TOO_SHORT | TOO_LONG | TWO_CONTS;

/// Table indexed by the high nibble of the byte before the one being judged.
pub const LEAD_HIGH: [u8; 16] = [
    // 0xxxxxxx — ASCII, so the byte after it may not be a continuation.
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    TOO_LONG,
    // 10xxxxxx — itself a continuation.
    TWO_CONTS,
    TWO_CONTS,
    TWO_CONTS,
    TWO_CONTS,
    // 1100xxxx — a two-byte lead, `0xC0` / `0xC1` overlong.
    TOO_SHORT | OVERLONG_2,
    // 1101xxxx — a two-byte lead.
    TOO_SHORT,
    // 1110xxxx — a three-byte lead, which may be overlong or a surrogate.
    TOO_SHORT | OVERLONG_3 | SURROGATE,
    // 1111xxxx — a four-byte lead, which may be overlong or too large.
    TOO_SHORT | TOO_LARGE | TOO_LARGE_1000 | OVERLONG_4,
];

/// Table indexed by the low nibble of the byte before the one being judged.
pub const LEAD_LOW: [u8; 16] = [
    CARRY | OVERLONG_2 | OVERLONG_3 | OVERLONG_4,
    CARRY | OVERLONG_2,
    CARRY,
    CARRY,
    CARRY | TOO_LARGE,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    // 0xED — a three-byte lead whose second byte may reach the surrogates.
    CARRY | TOO_LARGE | TOO_LARGE_1000 | SURROGATE,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
    CARRY | TOO_LARGE | TOO_LARGE_1000,
];

/// Table indexed by the high nibble of the byte being judged.
pub const SECOND_HIGH: [u8; 16] = [
    // 0xxxxxxx — not a continuation, so every lead before it is too short.
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    // 1000xxxx
    TOO_LONG | OVERLONG_2 | TWO_CONTS | OVERLONG_3 | TOO_LARGE_1000 | OVERLONG_4,
    // 1001xxxx
    TOO_LONG | OVERLONG_2 | TWO_CONTS | OVERLONG_3 | TOO_LARGE,
    // 101xxxxx
    TOO_LONG | OVERLONG_2 | TWO_CONTS | SURROGATE | TOO_LARGE,
    TOO_LONG | OVERLONG_2 | TWO_CONTS | SURROGATE | TOO_LARGE,
    // 11xxxxxx — a lead byte, so again every lead before it is too short.
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
    TOO_SHORT,
];

/// What a lead byte at or above `0xE0` becomes after this subtraction: the
/// high bit, marking the byte as one that demands a third byte.
pub const THIRD_BYTE_BIAS: u8 = 0xE0 - 0x80;
/// The same for a lead byte at or above `0xF0`, which demands a fourth.
pub const FOURTH_BYTE_BIAS: u8 = 0xF0 - 0x80;

/// The first byte of the only sequence that is well-formed UTF-8 and still
/// forbidden by the `Char` production: U+FFFE and U+FFFF.
pub const NONCHAR_LEAD: u8 = 0xEF;
/// Its second byte.
pub const NONCHAR_SECOND: u8 = 0xBF;
/// Its third byte, once the low bit is masked off — `0xBE` and `0xBF` are the
/// two that spell a noncharacter.
pub const NONCHAR_THIRD: u8 = 0xBE;
/// The mask that folds those two third bytes together.
pub const NONCHAR_THIRD_MASK: u8 = 0xFE;

/// How many bytes before a block a kernel is handed: one vector of the
/// widest kind, which is all any of them shifts in.
pub const PRECEDING: usize = 32;

/// A kernel that checks one block, given the bytes just before it.
///
/// Bit `n` of the result is set when byte `n` of `block` takes part in
/// something the document may not contain.
pub type Validator = fn(prev: &[u8; PRECEDING], block: &[u8; BLOCK]) -> u64;

/// Judge one byte from the three table entries and the two bytes before it.
#[inline]
fn judge(prev3: u8, prev2: u8, prev1: u8, byte: u8) -> u8 {
    let special = LEAD_HIGH[(prev1 >> 4) as usize]
        & LEAD_LOW[(prev1 & 0x0F) as usize]
        & SECOND_HIGH[(byte >> 4) as usize];
    // A byte that follows a three- or four-byte lead by one or two places
    // must be a continuation. `TWO_CONTS` in `special` says it is; the high
    // bit here says it has to be. They must agree.
    let must_continue =
        (prev2.saturating_sub(THIRD_BYTE_BIAS) | prev3.saturating_sub(FOURTH_BYTE_BIAS)) & 0x80;
    let malformed = must_continue ^ special;
    let noncharacter = prev2 == NONCHAR_LEAD
        && prev1 == NONCHAR_SECOND
        && byte & NONCHAR_THIRD_MASK == NONCHAR_THIRD;
    malformed | u8::from(noncharacter)
}

/// The portable checker, and the definition every kernel answers to.
#[must_use]
pub fn validate_scalar(prev: &[u8; PRECEDING], block: &[u8; BLOCK]) -> u64 {
    let byte_before = |at: usize, back: usize| -> u8 {
        if at >= back {
            block[at - back]
        } else {
            prev[PRECEDING + at - back]
        }
    };
    let mut error = 0u64;
    for (at, &byte) in block.iter().enumerate() {
        if judge(
            byte_before(at, 3),
            byte_before(at, 2),
            byte_before(at, 1),
            byte,
        ) != 0
        {
            error |= 1u64 << at;
        }
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chars::is_char;
    use crate::encoding::{Encoding, Utf8};

    /// Run the checker over a whole document the way the index does, and say
    /// only whether it found anything.
    fn vector_rejects(document: &[u8]) -> bool {
        let mut prev = [0u8; PRECEDING];
        let mut base = 0;
        loop {
            let mut block = [b' '; BLOCK];
            let take = document.len().saturating_sub(base).min(BLOCK);
            block[..take].copy_from_slice(&document[base..base + take]);
            if validate_scalar(&prev, &block) != 0 {
                return true;
            }
            prev.copy_from_slice(&block[BLOCK - PRECEDING..]);
            base += BLOCK;
            // One block of spaces past the end closes any sequence the
            // document left open.
            if base > document.len() {
                return false;
            }
        }
    }

    /// Decode the document the slow, obvious way.
    fn decoder_rejects(document: &[u8]) -> bool {
        let mut pos = 0;
        while pos < document.len() {
            if document[pos] < 0x80 {
                pos += 1;
                continue;
            }
            match Utf8::decode(document, pos) {
                Ok((code, width)) => {
                    if !is_char(code) {
                        return true;
                    }
                    pos += width;
                }
                Err(_) => return true,
            }
        }
        false
    }

    fn assert_same_verdict(document: &[u8], note: &str) {
        assert_eq!(
            vector_rejects(document),
            decoder_rejects(document),
            "{note}: {document:02X?}"
        );
    }

    #[test]
    fn every_two_byte_sequence_is_judged_as_the_decoder_judges_it() {
        for first in 0x80u16..=0xFF {
            for second in 0u16..=0xFF {
                let doc = [b'<', first as u8, second as u8, b'>'];
                assert_same_verdict(&doc, "two bytes");
            }
        }
    }

    #[test]
    fn every_three_byte_sequence_under_a_three_byte_lead_agrees() {
        for lead in 0xE0u16..=0xEF {
            for second in 0u16..=0xFF {
                for third in [0x00u8, 0x7F, 0x80, 0x8F, 0x90, 0xA0, 0xBE, 0xBF, 0xC0, 0xFF] {
                    let doc = [lead as u8, second as u8, third];
                    assert_same_verdict(&doc, "three bytes");
                }
            }
        }
    }

    #[test]
    fn every_four_byte_sequence_under_a_four_byte_lead_agrees() {
        for lead in 0xF0u16..=0xFF {
            for second in 0u16..=0xFF {
                for third in [0x80u8, 0xBF, 0x20] {
                    for fourth in [0x80u8, 0xBF, 0x20] {
                        let doc = [lead as u8, second as u8, third, fourth];
                        assert_same_verdict(&doc, "four bytes");
                    }
                }
            }
        }
    }

    #[test]
    fn a_sequence_the_document_ends_inside_is_rejected() {
        for truncated in [
            &b"\xC3"[..],
            b"\xE2\x82",
            b"\xE2",
            b"\xF0\x9F\x98",
            b"\xF0\x9F",
            b"\xF0",
        ] {
            assert!(vector_rejects(truncated), "{truncated:02X?}");
        }
    }

    #[test]
    fn the_two_noncharacters_are_rejected_and_their_neighbours_are_not() {
        assert!(vector_rejects("\u{FFFE}".as_bytes()));
        assert!(vector_rejects("\u{FFFF}".as_bytes()));
        assert!(!vector_rejects("\u{FFFD}".as_bytes()));
        // Every other plane's last two code points are legal in XML 1.0.
        assert!(!vector_rejects("\u{1FFFE}".as_bytes()));
        assert!(!vector_rejects("\u{10FFFF}".as_bytes()));
    }

    #[test]
    fn text_in_scripts_that_need_every_sequence_length_passes() {
        for text in [
            "ascii only",
            "café société",
            "日本語のテキスト",
            "русский текст",
            "emoji 😀🚀 and math ∑∫",
            "\u{10348}\u{2070E}",
        ] {
            assert!(!vector_rejects(text.as_bytes()), "{text}");
            // …and again at every offset across a block boundary.
            for pad in 60..70 {
                let mut doc = "x".repeat(pad).into_bytes();
                doc.extend_from_slice(text.as_bytes());
                assert!(!vector_rejects(&doc), "{text} at {pad}");
            }
        }
    }

    #[test]
    fn a_broken_sequence_across_a_block_boundary_is_still_caught() {
        for pad in 60..70usize {
            for broken in [
                &b"\xE2\x82"[..],
                b"\xED\xA0\x80",
                b"\xC0\xAF",
                b"\xF5\x80\x80\x80",
            ] {
                let mut doc = "x".repeat(pad).into_bytes();
                doc.extend_from_slice(broken);
                doc.extend_from_slice(b"<a/>");
                assert_same_verdict(&doc, "across a boundary");
            }
        }
    }
}
