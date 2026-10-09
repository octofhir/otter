//! Injective spelling of WTF-16 text as UTF-8.
//!
//! A JS string is WTF-16 and may hold an unpaired surrogate; a Rust `str`
//! cannot. Two consumers need a string's exact code units inside a `str`:
//! the parser, which takes `eval` and `Function` source, and property keys,
//! whose spellings the name interner, shapes and dictionaries store as `str`.
//! Converting lossily turns every unpaired surrogate into U+FFFD, so distinct
//! strings collapse: `o["\uD800"]` would read `o["�"]`.
//!
//! The escape spells an unpaired surrogate as [`MARKER`] followed by a
//! Private Use Area character, and a literal marker as a doubled marker.
//! Every other code unit, well-formed pairs included, spells as itself, so
//! text holding neither a marker nor an unpaired surrogate encodes to its
//! ordinary UTF-8.
//!
//! # Contents
//! - [`escaped_chars`], [`encode`], [`encoded_len`] — WTF-16 to a spelling.
//! - [`units`] — the code units a spelling stands for, read in place.
//! - [`is_escaped`] — whether a spelling differs from its plain UTF-8 reading.
//! - [`decode_units`] — undo the escape in a code-unit buffer.
//!
//! # Invariants
//! - [`MARKER`] never survives unescaped: each occurrence in a spelling
//!   introduces exactly one escape. The encoding is therefore injective, and
//!   a concatenation of spellings decodes as the concatenation of the texts.
//! - [`units`] over [`encode`]`(text)` yields exactly `text`.
//!
//! # See also
//! - `eval_source` — the parser end of the escape.
//! - `property_atom::NameInterner` — hashes and compares spellings by
//!   [`units`], so a string's code units find its escaped spelling in place.

/// Introduces an escape. A literal occurrence doubles.
pub(crate) const MARKER: u16 = 0xE000;

const MARKER_CHAR: char = '\u{E000}';

/// `MARKER` followed by `SURROGATE_BASE + (unit - 0xD800)` is one unpaired
/// surrogate. The range lands inside the Private Use Area, so the escaped
/// form is always a well-formed code point.
const SURROGATE_BASE: u16 = 0xE001;

const fn is_high_surrogate(unit: u16) -> bool {
    matches!(unit, 0xD800..=0xDBFF)
}

const fn is_low_surrogate(unit: u16) -> bool {
    matches!(unit, 0xDC00..=0xDFFF)
}

const fn is_surrogate(unit: u16) -> bool {
    matches!(unit, 0xD800..=0xDFFF)
}

/// Whether `units` spell differently under the escape than as plain UTF-16.
fn needs_escape(units: &[u16]) -> bool {
    units
        .iter()
        .any(|&unit| unit == MARKER || is_surrogate(unit))
}

/// The escaped spelling of `units`, one character at a time.
pub(crate) fn escaped_chars(units: &[u16]) -> EscapedChars<'_> {
    EscapedChars {
        units,
        index: 0,
        pending: None,
    }
}

/// Iterator returned by [`escaped_chars`].
pub(crate) struct EscapedChars<'a> {
    units: &'a [u16],
    index: usize,
    pending: Option<char>,
}

impl Iterator for EscapedChars<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        if let Some(pending) = self.pending.take() {
            return Some(pending);
        }
        let unit = *self.units.get(self.index)?;
        self.index += 1;
        if unit == MARKER {
            self.pending = Some(MARKER_CHAR);
            return Some(MARKER_CHAR);
        }
        if is_high_surrogate(unit)
            && let Some(&low) = self.units.get(self.index)
            && is_low_surrogate(low)
        {
            self.index += 1;
            let scalar = 0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
            return char::from_u32(scalar);
        }
        if is_surrogate(unit) {
            self.pending = char::from_u32(u32::from(SURROGATE_BASE + (unit - 0xD800)));
            return Some(MARKER_CHAR);
        }
        char::from_u32(u32::from(unit))
    }
}

/// The escaped spelling of `units`.
#[must_use]
pub(crate) fn encode(units: &[u16]) -> String {
    // Almost every text needs no escape, and checking is one pass over code
    // units that a conversion walks anyway.
    if !needs_escape(units) {
        return String::from_utf16(units).expect("no surrogate remains to be unpaired");
    }
    escaped_chars(units).collect()
}

/// UTF-8 length of the escaped spelling of `units`.
#[must_use]
pub(crate) fn encoded_len(units: &[u16]) -> usize {
    escaped_chars(units).map(char::len_utf8).sum()
}

/// Whether `spelling` holds an escape, so its code units differ from its
/// plain UTF-16 encoding.
#[must_use]
pub(crate) fn is_escaped(spelling: &str) -> bool {
    !spelling.is_ascii() && spelling.contains(MARKER_CHAR)
}

/// The code units `spelling` stands for.
pub(crate) fn units(spelling: &str) -> SpellingUnits<'_> {
    SpellingUnits {
        chars: spelling.chars(),
        pending: [0; 2],
        pending_len: 0,
    }
}

/// Iterator returned by [`units`].
pub(crate) struct SpellingUnits<'a> {
    chars: std::str::Chars<'a>,
    /// Units still owed, last one first.
    pending: [u16; 2],
    pending_len: usize,
}

impl SpellingUnits<'_> {
    fn owe(&mut self, character: char) {
        let mut buffer = [0u16; 2];
        let encoded = character.encode_utf16(&mut buffer);
        for &unit in encoded.iter().rev() {
            self.pending[self.pending_len] = unit;
            self.pending_len += 1;
        }
    }
}

impl Iterator for SpellingUnits<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        if self.pending_len != 0 {
            self.pending_len -= 1;
            return Some(self.pending[self.pending_len]);
        }
        let character = self.chars.next()?;
        if character != MARKER_CHAR {
            let mut buffer = [0u16; 2];
            let encoded = character.encode_utf16(&mut buffer);
            if encoded.len() == 2 {
                self.pending[0] = encoded[1];
                self.pending_len = 1;
            }
            return Some(encoded[0]);
        }
        match self.chars.next() {
            Some(MARKER_CHAR) => Some(MARKER),
            Some(escaped)
                if (u32::from(SURROGATE_BASE)..u32::from(SURROGATE_BASE) + 0x800)
                    .contains(&u32::from(escaped)) =>
            {
                Some(0xD800 + (u32::from(escaped) as u16 - SURROGATE_BASE))
            }
            // Unreachable for an encoded spelling; a stray marker reads as
            // itself rather than swallowing what follows it.
            Some(other) => {
                self.owe(other);
                Some(MARKER)
            }
            None => Some(MARKER),
        }
    }
}

/// Undo the escape in a buffer of code units that spells escaped text.
pub(crate) fn decode_units(units: &mut Vec<u16>) {
    if !units.contains(&MARKER) {
        return;
    }
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if unit != MARKER {
            out.push(unit);
            index += 1;
            continue;
        }
        match units.get(index + 1).copied() {
            Some(MARKER) => out.push(MARKER),
            Some(escaped) if (SURROGATE_BASE..SURROGATE_BASE + 0x800).contains(&escaped) => {
                out.push(0xD800 + (escaped - SURROGATE_BASE));
            }
            // Unreachable for escaped text; leaving the marker in place keeps
            // an unexpected input readable rather than truncated.
            _ => {
                out.push(MARKER);
                index += 1;
                continue;
            }
        }
        index += 2;
    }
    *units = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(text: &[u16]) -> Vec<u16> {
        units(&encode(text)).collect()
    }

    #[test]
    fn unpaired_surrogates_spell_distinctly() {
        let spellings: Vec<String> = [0xD800u16, 0xDBFF, 0xDC00, 0xDFFF, 0xFFFD]
            .iter()
            .map(|&unit| encode(&[unit]))
            .collect();
        for (index, spelling) in spellings.iter().enumerate() {
            assert!(!spellings[index + 1..].contains(spelling));
        }
        for unit in [0xD800u16, 0xDBFF, 0xDC00, 0xDFFF] {
            let text = ['k' as u16, unit, 'v' as u16];
            assert!(is_escaped(&encode(&text)));
            assert_eq!(round_trip(&text), text);
            assert_eq!(encoded_len(&text), encode(&text).len());
        }
    }

    #[test]
    fn markers_pairs_and_plain_text_round_trip() {
        let marker: Vec<u16> = "a\u{E000}b".encode_utf16().chain([0xD800]).collect();
        assert_eq!(round_trip(&marker), marker);
        let lookalike: Vec<u16> = "\u{E000}\u{E001}".encode_utf16().collect();
        assert_ne!(encode(&lookalike), encode(&[0xD800]));
        assert_eq!(round_trip(&lookalike), lookalike);
        let pair: Vec<u16> = "x\u{1F600}y".encode_utf16().collect();
        assert_eq!(encode(&pair), "x\u{1F600}y");
        assert!(!is_escaped(&encode(&pair)));
        assert_eq!(round_trip(&pair), pair);
        // A high surrogate before a pair escapes alone.
        let split = [0xD800u16, 0xD83D, 0xDE00];
        assert_eq!(round_trip(&split), split);
    }

    #[test]
    fn decoding_a_unit_buffer_matches_reading_the_spelling() {
        let text = [0x61u16, 0xDC00, MARKER, 0xD83D, 0xDE00, 0xDBFF];
        let mut buffer: Vec<u16> = encode(&text).encode_utf16().collect();
        decode_units(&mut buffer);
        assert_eq!(buffer, text);
    }
}
