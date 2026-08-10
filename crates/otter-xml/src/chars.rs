//! Character classification for XML 1.0 (Fifth Edition).
//!
//! # Contents
//! - [`is_char`] — the `Char` production: the code points a document may contain.
//! - [`is_whitespace`] — the `S` production.
//! - [`is_name_start`] / [`is_name_char`] — the `NameStartChar` / `NameChar`
//!   productions, used for element, attribute, entity and target names.
//!
//! # Invariants
//! - The classifiers take a Unicode scalar value, never a code unit: callers
//!   decode first, so the answers do not depend on the document's encoding.
//! - The ASCII half of the name productions is answered from a bitmap; only
//!   code points above `0x7F` reach the range ladder.
//! - Surrogate halves are not scalar values and are rejected by [`is_char`].
//!
//! # See also
//! - <https://www.w3.org/TR/2008/REC-xml-20081126/#charsets>
//! - <https://www.w3.org/TR/2008/REC-xml-20081126/#NT-Name>

/// One bit per ASCII code point that may start a `Name`: `:`, `_`, and the
/// two letter runs. Bit `n` of word `n / 64` answers code point `n`.
const ASCII_NAME_START: [u64; 2] = {
    let mut bits = [0u64; 2];
    bits[0] |= 1 << b':';
    bits[1] |= 1 << (b'_' - 64);
    let mut c = b'A';
    while c <= b'Z' {
        bits[1] |= 1 << (c - 64);
        c += 1;
    }
    let mut c = b'a';
    while c <= b'z' {
        bits[1] |= 1 << (c - 64);
        c += 1;
    }
    bits
};

/// [`ASCII_NAME_START`] plus the ASCII code points a `Name` may continue with:
/// `-`, `.` and the digits.
const ASCII_NAME_CHAR: [u64; 2] = {
    let mut bits = ASCII_NAME_START;
    bits[0] |= 1 << b'-';
    bits[0] |= 1 << b'.';
    let mut c = b'0';
    while c <= b'9' {
        bits[0] |= 1 << c;
        c += 1;
    }
    bits
};

#[inline(always)]
const fn ascii_bit(bits: &[u64; 2], code: u32) -> bool {
    bits[(code >> 6) as usize] & (1u64 << (code & 63)) != 0
}

/// Whether `code` is a legal document character (the `Char` production).
///
/// Excludes the C0 controls other than tab, newline and carriage return, the
/// surrogate range, and U+FFFE / U+FFFF.
#[inline]
#[must_use]
pub const fn is_char(code: u32) -> bool {
    matches!(code, 0x9 | 0xA | 0xD)
        || (0x20 <= code && code <= 0xD7FF)
        || (0xE000 <= code && code <= 0xFFFD)
        || (0x1_0000 <= code && code <= 0x10_FFFF)
}

/// Whether `code` is XML whitespace (the `S` production).
#[inline]
#[must_use]
pub const fn is_whitespace(code: u32) -> bool {
    matches!(code, 0x20 | 0x9 | 0xD | 0xA)
}

/// Whether `code` may start a `Name`.
#[inline]
#[must_use]
pub const fn is_name_start(code: u32) -> bool {
    if code < 0x80 {
        return ascii_bit(&ASCII_NAME_START, code);
    }
    (0xC0 <= code && code <= 0xD6)
        || (0xD8 <= code && code <= 0xF6)
        || (0xF8 <= code && code <= 0x2FF)
        || (0x370 <= code && code <= 0x37D)
        || (0x37F <= code && code <= 0x1FFF)
        || (0x200C <= code && code <= 0x200D)
        || (0x2070 <= code && code <= 0x218F)
        || (0x2C00 <= code && code <= 0x2FEF)
        || (0x3001 <= code && code <= 0xD7FF)
        || (0xF900 <= code && code <= 0xFDCF)
        || (0xFDF0 <= code && code <= 0xFFFD)
        || (0x1_0000 <= code && code <= 0xE_FFFF)
}

/// Whether `code` may continue a `Name`.
#[inline]
#[must_use]
pub const fn is_name_char(code: u32) -> bool {
    if code < 0x80 {
        return ascii_bit(&ASCII_NAME_CHAR, code);
    }
    is_name_start(code)
        || code == 0xB7
        || (0x300 <= code && code <= 0x36F)
        || (0x203F <= code && code <= 0x2040)
}

/// Whether `code` may appear in a public identifier, whose alphabet the
/// grammar narrows to what every character set of the day spelled the same
/// way.
#[inline]
#[must_use]
pub const fn is_pubid_char(code: u32) -> bool {
    matches!(code, 0x20 | 0xD | 0xA)
        || matches!(code, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A)
        || matches!(
            code,
            0x2D | 0x27
                | 0x28
                | 0x29
                | 0x2B
                | 0x2C
                | 0x2E
                | 0x2F
                | 0x3A
                | 0x3D
                | 0x3F
                | 0x3B
                | 0x21
                | 0x2A
                | 0x23
                | 0x40
                | 0x24
                | 0x5F
                | 0x25
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_identifiers_keep_to_their_own_alphabet() {
        assert!(is_pubid_char(u32::from(b'A')) && is_pubid_char(u32::from(b'9')));
        assert!(is_pubid_char(u32::from(b'-')) && is_pubid_char(u32::from(b'\'')));
        assert!(is_pubid_char(0x20) && is_pubid_char(0xA) && is_pubid_char(0xD));
        assert!(!is_pubid_char(0x9));
        assert!(!is_pubid_char(u32::from(b'<')) && !is_pubid_char(u32::from(b'"')));
        assert!(!is_pubid_char(u32::from(b'[')) && !is_pubid_char(u32::from(b'\\')));
        assert!(!is_pubid_char(0xE9));
    }

    #[test]
    fn char_production_excludes_controls_surrogates_and_noncharacters() {
        assert!(is_char(0x9) && is_char(0xA) && is_char(0xD));
        assert!(!is_char(0x0) && !is_char(0x1) && !is_char(0xB) && !is_char(0x1F));
        assert!(is_char(0x20) && is_char(0xD7FF));
        assert!(!is_char(0xD800) && !is_char(0xDFFF));
        assert!(is_char(0xE000) && is_char(0xFFFD));
        assert!(!is_char(0xFFFE) && !is_char(0xFFFF));
        // Fifth Edition excludes only the BMP pair; higher planes keep theirs.
        assert!(is_char(0x1_0000) && is_char(0x1_FFFE) && is_char(0x10_FFFF));
        assert!(!is_char(0x11_0000));
    }

    #[test]
    fn ascii_name_bitmaps_match_the_grammar() {
        for code in 0..0x80u32 {
            let c = code as u8 as char;
            let start = c == ':' || c == '_' || c.is_ascii_alphabetic();
            let cont = start || c == '-' || c == '.' || c.is_ascii_digit();
            assert_eq!(is_name_start(code), start, "start {c:?}");
            assert_eq!(is_name_char(code), cont, "char {c:?}");
        }
    }

    #[test]
    fn non_ascii_name_ranges() {
        assert!(is_name_start(0xC0) && !is_name_start(0xD7) && is_name_start(0xD8));
        assert!(!is_name_start(0xB7) && is_name_char(0xB7));
        assert!(!is_name_start(0x300) && is_name_char(0x300));
        assert!(is_name_start(0x3042));
        assert!(!is_name_start(0xE_FFFF + 1));
        // A name character is never something the document may not contain.
        for code in [0xC0u32, 0xB7, 0x300, 0x3042, 0x1_0000] {
            assert!(is_char(code));
        }
    }
}
