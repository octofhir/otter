//! Code-unit encodings and the sniffing that picks one for byte input.
//!
//! # Contents
//! - [`Unit`] — a code unit the scanner can compare against ASCII literals.
//! - [`Encoding`] — decoding one scalar value from a unit slice.
//! - [`Utf8`], [`Latin1`], [`Utf16`] — the three forms a document is read in.
//! - [`Charset`] / [`sniff`] — byte-order marks and the `encoding` declaration.
//! - [`decode_utf16`] — byte input to UTF-16 code units.
//!
//! # Invariants
//! - Decoding never transcodes: the scanner is generic over [`Encoding`], so a
//!   document is read in whatever form it arrived in. The one exception is byte
//!   input that declares UTF-16, where the byte order must be resolved first.
//! - Every structural character of XML is ASCII, so [`Unit::value`] is enough
//!   to drive the scanner; [`Encoding::decode`] is reached only for content.
//! - `decode` rejects anything that is not a scalar value, including surrogate
//!   halves, overlong forms and code points above U+10FFFF.
//!
//! # See also
//! - <https://www.w3.org/TR/2008/REC-xml-20081126/#charencoding>

use crate::error::{Error, ErrorKind, Result};

/// One code unit of a document's text.
pub trait Unit: Copy + Eq + core::fmt::Debug {
    /// Whether a unit is wider than one byte.
    const WIDE: bool;

    /// The unit's numeric value, widened.
    fn value(self) -> u32;

    /// The unit spelling an ASCII byte. Every encoding here spells ASCII in
    /// one unit, which is what lets one scanner drive all three.
    fn from_ascii(ascii: u8) -> Self;

    /// Whether the unit is the given ASCII byte.
    #[inline(always)]
    fn is(self, ascii: u8) -> bool {
        self.value() == ascii as u32
    }
}

impl Unit for u8 {
    const WIDE: bool = false;

    #[inline(always)]
    fn value(self) -> u32 {
        self as u32
    }

    #[inline(always)]
    fn from_ascii(ascii: u8) -> Self {
        ascii
    }
}

impl Unit for u16 {
    const WIDE: bool = true;

    #[inline(always)]
    fn value(self) -> u32 {
        self as u32
    }

    #[inline(always)]
    fn from_ascii(ascii: u8) -> Self {
        ascii as u16
    }
}

/// How a slice of code units spells scalar values.
pub trait Encoding {
    /// The code unit this encoding is built from.
    type Unit: Unit;

    /// Decode the scalar starting at `pos`, returning it and how many units it
    /// took. `pos` must be less than `units.len()`.
    fn decode(units: &[Self::Unit], pos: usize) -> Result<(u32, usize)>;

    /// Append `code` to `out`, or report that this encoding cannot spell it.
    ///
    /// Only ISO-8859-1 can fail, and only for a character reference naming
    /// something above U+00FF; the scanner then widens the run to UTF-16.
    fn encode(code: u32, out: &mut Vec<Self::Unit>) -> bool;

    /// Append this document's structural positions to `out`, checking as it
    /// goes that every unit spells a character a document may contain.
    ///
    /// Each encoding picks the producer that suits its units: the byte
    /// encodings classify a block at a time, UTF-16 walks its units.
    ///
    /// # Errors
    /// Returns the first ill-formed or forbidden character.
    fn index_into(document: &[Self::Unit], out: &mut Vec<u32>) -> Result<()>;
}

/// UTF-8, the default for byte input.
#[derive(Debug, Clone, Copy)]
pub struct Utf8;

/// ISO-8859-1, where every byte is the code point of the same value.
#[derive(Debug, Clone, Copy)]
pub struct Latin1;

/// UTF-16, in the host's order once the byte order has been resolved.
#[derive(Debug, Clone, Copy)]
pub struct Utf16;

impl Encoding for Utf8 {
    type Unit = u8;

    #[inline]
    fn decode(units: &[u8], pos: usize) -> Result<(u32, usize)> {
        let malformed = || Error::new(ErrorKind::MalformedEncoding, pos);
        let lead = units[pos];
        if lead < 0x80 {
            return Ok((lead as u32, 1));
        }
        let width = match lead {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            // 0x80..=0xC1 is either a stray continuation or an overlong lead.
            _ => return Err(malformed()),
        };
        if pos + width > units.len() {
            return Err(malformed());
        }
        let mut code = (lead as u32) & (0x7F >> width);
        for &unit in &units[pos + 1..pos + width] {
            if unit & 0xC0 != 0x80 {
                return Err(malformed());
            }
            code = (code << 6) | (unit as u32 & 0x3F);
        }
        // Reject the shortest-form and range violations the width alone allows.
        let shortest = match width {
            2 => 0x80,
            3 => 0x800,
            _ => 0x1_0000,
        };
        if code < shortest || code > 0x10_FFFF || (0xD800..=0xDFFF).contains(&code) {
            return Err(malformed());
        }
        Ok((code, width))
    }

    fn encode(code: u32, out: &mut Vec<u8>) -> bool {
        match code {
            0..=0x7F => out.push(code as u8),
            0x80..=0x7FF => {
                out.extend_from_slice(&[0xC0 | (code >> 6) as u8, 0x80 | (code & 0x3F) as u8])
            }
            0x800..=0xFFFF => out.extend_from_slice(&[
                0xE0 | (code >> 12) as u8,
                0x80 | ((code >> 6) & 0x3F) as u8,
                0x80 | (code & 0x3F) as u8,
            ]),
            _ => out.extend_from_slice(&[
                0xF0 | (code >> 18) as u8,
                0x80 | ((code >> 12) & 0x3F) as u8,
                0x80 | ((code >> 6) & 0x3F) as u8,
                0x80 | (code & 0x3F) as u8,
            ]),
        }
        true
    }

    fn index_into(document: &[u8], out: &mut Vec<u32>) -> Result<()> {
        crate::index::bytes(document, true, out)
    }
}

impl Encoding for Latin1 {
    type Unit = u8;

    #[inline]
    fn decode(units: &[u8], pos: usize) -> Result<(u32, usize)> {
        Ok((units[pos] as u32, 1))
    }

    fn encode(code: u32, out: &mut Vec<u8>) -> bool {
        if code > 0xFF {
            return false;
        }
        out.push(code as u8);
        true
    }

    fn index_into(document: &[u8], out: &mut Vec<u32>) -> Result<()> {
        // Every byte is a character in its own right, so nothing above ASCII
        // needs a second look.
        crate::index::bytes(document, false, out)
    }
}

impl Encoding for Utf16 {
    type Unit = u16;

    #[inline]
    fn decode(units: &[u16], pos: usize) -> Result<(u32, usize)> {
        let malformed = || Error::new(ErrorKind::MalformedEncoding, pos);
        let lead = units[pos] as u32;
        if !(0xD800..=0xDFFF).contains(&lead) {
            return Ok((lead, 1));
        }
        if lead >= 0xDC00 {
            return Err(malformed());
        }
        let trail = units.get(pos + 1).map_or(0, |&unit| unit as u32);
        if !(0xDC00..=0xDFFF).contains(&trail) {
            return Err(malformed());
        }
        Ok((0x1_0000 + ((lead - 0xD800) << 10) + (trail - 0xDC00), 2))
    }

    fn encode(code: u32, out: &mut Vec<u16>) -> bool {
        push_utf16(code, out);
        true
    }

    fn index_into(document: &[u16], out: &mut Vec<u32>) -> Result<()> {
        crate::index::units::<Self>(document, out)
    }
}

/// Append `code` to a UTF-16 buffer, as a pair when it needs one.
pub fn push_utf16(code: u32, out: &mut Vec<u16>) {
    if code < 0x1_0000 {
        out.push(code as u16);
        return;
    }
    let shifted = code - 0x1_0000;
    out.push(0xD800 + (shifted >> 10) as u16);
    out.push(0xDC00 + (shifted & 0x3FF) as u16);
}

/// Which encoding a byte input turned out to be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    /// UTF-8, declared or by default.
    Utf8,
    /// ISO-8859-1.
    Latin1,
    /// UTF-16, big-endian.
    Utf16Be,
    /// UTF-16, little-endian.
    Utf16Le,
}

/// A byte input's encoding and the offset its text starts at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sniffed {
    /// The encoding to read the document in.
    pub charset: Charset,
    /// How many leading bytes the byte-order mark took.
    pub bom_len: usize,
}

/// Decide how to read `bytes`, from its byte-order mark and, failing that, the
/// `encoding` pseudo-attribute of its XML declaration.
///
/// # Errors
/// Returns [`ErrorKind::UnsupportedEncoding`] for a declared encoding this
/// parser does not read, and [`ErrorKind::BadDeclaration`] when a declaration
/// claims UTF-16 without a byte-order mark to give it an order.
pub fn sniff(bytes: &[u8]) -> Result<Sniffed> {
    let mark = |charset, bom_len| Ok(Sniffed { charset, bom_len });
    match bytes {
        [0xFE, 0xFF, ..] => return mark(Charset::Utf16Be, 2),
        [0xFF, 0xFE, ..] => return mark(Charset::Utf16Le, 2),
        [0xEF, 0xBB, 0xBF, ..] => return mark(Charset::Utf8, 3),
        // A declaration in UTF-16 with no mark still spells `<?` in its units.
        [0x00, 0x3C, 0x00, 0x3F, ..] => return mark(Charset::Utf16Be, 0),
        [0x3C, 0x00, 0x3F, 0x00, ..] => return mark(Charset::Utf16Le, 0),
        _ => {}
    }
    let Some(name) = declared_encoding(bytes) else {
        return mark(Charset::Utf8, 0);
    };
    let charset = match name.to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" | "us-ascii" | "ascii" => Charset::Utf8,
        "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1" => Charset::Latin1,
        "utf-16" | "utf16" | "utf-16le" | "utf-16be" => {
            // A byte-order mark would have been seen above.
            return Err(Error::new(
                ErrorKind::BadDeclaration("UTF-16 input needs a byte-order mark"),
                0,
            ));
        }
        _ => return Err(Error::new(ErrorKind::UnsupportedEncoding(name), 0)),
    };
    mark(charset, 0)
}

/// Read the `encoding` pseudo-attribute out of a leading `<?xml … ?>`, if the
/// input starts with one. Syntax is only checked far enough to find the value;
/// the scanner validates the declaration properly later.
fn declared_encoding(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(256)];
    let rest = head.strip_prefix(b"<?xml")?;
    // The declaration must be followed by whitespace, else this is a PI named
    // `xmlfoo` and carries no encoding.
    if !matches!(rest.first(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        return None;
    }
    let end = rest.windows(2).position(|pair| pair == b"?>")?;
    let decl = &rest[..end];
    let at = decl.windows(8).position(|window| window == b"encoding")?;
    let mut cursor = at + 8;
    while matches!(decl.get(cursor), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        cursor += 1;
    }
    if decl.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor += 1;
    while matches!(decl.get(cursor), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        cursor += 1;
    }
    let quote = *decl.get(cursor)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    cursor += 1;
    let value = &decl[cursor..];
    let len = value.iter().position(|&byte| byte == quote)?;
    core::str::from_utf8(&value[..len]).ok().map(str::to_owned)
}

/// Turn byte input into UTF-16 code units in the given order.
///
/// # Errors
/// Returns [`ErrorKind::MalformedEncoding`] when the input has an odd length.
pub fn decode_utf16(bytes: &[u8], big_endian: bool) -> Result<Vec<u16>> {
    if !bytes.len().is_multiple_of(2) {
        return Err(Error::new(ErrorKind::MalformedEncoding, bytes.len() - 1));
    }
    Ok(bytes
        .chunks_exact(2)
        .map(|pair| {
            let (hi, lo) = if big_endian {
                (pair[0], pair[1])
            } else {
                (pair[1], pair[0])
            };
            u16::from(hi) << 8 | u16::from(lo)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all_utf8(bytes: &[u8]) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < bytes.len() {
            let (code, width) = Utf8::decode(bytes, pos)?;
            out.push(code);
            pos += width;
        }
        Ok(out)
    }

    #[test]
    fn utf8_decodes_every_width_and_rejects_the_ill_formed() {
        assert_eq!(
            decode_all_utf8("aé漢🙂".as_bytes()).unwrap(),
            vec![0x61, 0xE9, 0x6F22, 0x1_F642]
        );
        for bad in [
            &b"\x80"[..],             // stray continuation
            &b"\xC0\xAF"[..],         // overlong slash
            &b"\xC1\xBF"[..],         // overlong DEL
            &b"\xE0\x80\xAF"[..],     // overlong, three bytes
            &b"\xED\xA0\x80"[..],     // surrogate half
            &b"\xF5\x80\x80\x80"[..], // above U+10FFFF
            &b"\xE2\x28\xA1"[..],     // bad continuation
            &b"\xE2\x82"[..],         // truncated
        ] {
            assert!(decode_all_utf8(bad).is_err(), "accepted {bad:x?}");
        }
    }

    #[test]
    fn latin1_is_the_identity_on_bytes() {
        for byte in 0..=255u8 {
            assert_eq!(Latin1::decode(&[byte], 0).unwrap(), (byte as u32, 1));
        }
    }

    #[test]
    fn utf16_pairs_surrogates_and_rejects_lone_halves() {
        let units: Vec<u16> = "a🙂".encode_utf16().collect();
        assert_eq!(Utf16::decode(&units, 0).unwrap(), (0x61, 1));
        assert_eq!(Utf16::decode(&units, 1).unwrap(), (0x1_F642, 2));
        assert!(Utf16::decode(&[0xD83D], 0).is_err());
        assert!(Utf16::decode(&[0xDE42], 0).is_err());
        assert!(Utf16::decode(&[0xD83D, 0x0041], 0).is_err());
    }

    #[test]
    fn byte_order_marks_win_over_declarations() {
        assert_eq!(
            sniff(b"\xEF\xBB\xBF<a/>").unwrap(),
            Sniffed {
                charset: Charset::Utf8,
                bom_len: 3
            }
        );
        assert_eq!(
            sniff(b"\xFE\xFF\x00<").unwrap(),
            Sniffed {
                charset: Charset::Utf16Be,
                bom_len: 2
            }
        );
        assert_eq!(
            sniff(b"\xFF\xFE<\x00").unwrap(),
            Sniffed {
                charset: Charset::Utf16Le,
                bom_len: 2
            }
        );
        assert_eq!(sniff(b"\x00<\x00?\x00x").unwrap().charset, Charset::Utf16Be);
        assert_eq!(sniff(b"<\x00?\x00x\x00").unwrap().charset, Charset::Utf16Le);
    }

    #[test]
    fn declared_encodings_map_or_are_refused() {
        let decl = |name: &str| format!("<?xml version=\"1.0\" encoding=\"{name}\"?><a/>");
        assert_eq!(
            sniff(decl("ISO-8859-1").as_bytes()).unwrap().charset,
            Charset::Latin1
        );
        assert_eq!(
            sniff(decl("utf-8").as_bytes()).unwrap().charset,
            Charset::Utf8
        );
        assert_eq!(
            sniff(decl("US-ASCII").as_bytes()).unwrap().charset,
            Charset::Utf8
        );
        assert!(matches!(
            sniff(decl("Shift_JIS").as_bytes()).unwrap_err().kind,
            ErrorKind::UnsupportedEncoding(name) if name == "Shift_JIS"
        ));
        assert!(matches!(
            sniff(decl("UTF-16").as_bytes()).unwrap_err().kind,
            ErrorKind::BadDeclaration(_)
        ));
        // No declaration, and a processing instruction that merely starts alike.
        assert_eq!(sniff(b"<a/>").unwrap().charset, Charset::Utf8);
        assert_eq!(
            sniff(b"<?xmlish encoding='x'?><a/>").unwrap().charset,
            Charset::Utf8
        );
    }

    #[test]
    fn utf16_byte_decoding_follows_the_order() {
        assert_eq!(decode_utf16(b"\x00a\x00b", true).unwrap(), vec![0x61, 0x62]);
        assert_eq!(
            decode_utf16(b"a\x00b\x00", false).unwrap(),
            vec![0x61, 0x62]
        );
        assert!(decode_utf16(b"\x00", true).is_err());
    }
}
