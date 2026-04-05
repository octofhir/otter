//! WTF-16 string type for JavaScript string values.
//!
//! JavaScript strings are sequences of UTF-16 code units (§6.1.4), which may
//! include lone surrogates (unpaired surrogate code units). This makes them
//! "WTF-16" — Wilfully Ill-Formed UTF-16.
//!
//! All major engines (V8, JavaScriptCore, SpiderMonkey) use WTF-16 internally.
//! Rust's `str` / `String` are valid UTF-8 only, which cannot represent lone
//! surrogates. This type bridges the gap.
//!
//! Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type>

use std::fmt;
use std::hash::{Hash, Hasher};

/// A JavaScript string stored as WTF-16 code units.
///
/// This is the canonical internal representation for JS string values.
/// It correctly handles lone surrogates, supplementary characters, and
/// all valid UTF-16 sequences.
///
/// §6.1.4 The String Type
/// Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type>
#[derive(Clone, Eq)]
pub struct JsString(Box<[u16]>);

impl JsString {
    // ── Construction ────────────────────────────────────────────────────

    /// Creates a `JsString` from a UTF-8 `&str`.
    ///
    /// All valid Unicode characters are correctly encoded to UTF-16.
    /// Since `&str` cannot contain lone surrogates, the result is always
    /// well-formed UTF-16.
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        JsString(s.encode_utf16().collect())
    }

    /// Creates a `JsString` from raw UTF-16 / WTF-16 code units.
    ///
    /// This preserves lone surrogates as-is — no validation or replacement.
    #[inline]
    pub fn from_utf16(units: impl Into<Box<[u16]>>) -> Self {
        JsString(units.into())
    }

    /// Creates a `JsString` from a `Vec<u16>` of WTF-16 code units.
    #[inline]
    pub fn from_utf16_vec(units: Vec<u16>) -> Self {
        JsString(units.into_boxed_slice())
    }

    /// Creates an empty `JsString`.
    #[inline]
    pub fn empty() -> Self {
        JsString(Box::new([]))
    }

    /// Decodes an oxc-encoded string with lone surrogates.
    ///
    /// oxc encodes lone surrogates in `StringLiteral.value` as `\u{FFFD}XXXX`
    /// where XXXX is the surrogate code unit in hex. The literal U+FFFD itself
    /// is encoded as `\u{FFFD}fffd`.
    ///
    /// See: <https://github.com/nicolo-ribaudo/tc39-proposal-structs> (oxc docs)
    pub fn from_oxc_encoded(value: &str) -> Self {
        let mut units: Vec<u16> = Vec::with_capacity(value.len());
        let chars: Vec<char> = value.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '\u{FFFD}' {
                // Check for the oxc encoding: \u{FFFD} followed by 4 hex digits
                if i + 4 < chars.len() {
                    let hex_str: String =
                        chars[i + 1..i + 5].iter().collect();
                    if let Ok(code_unit) = u16::from_str_radix(&hex_str, 16) {
                        units.push(code_unit);
                        i += 5;
                        continue;
                    }
                }
                // Not a valid encoding — just emit U+FFFD as-is
                units.push(0xFFFD);
                i += 1;
            } else {
                // Normal character — encode to UTF-16
                let ch = chars[i];
                let mut buf = [0u16; 2];
                let encoded = ch.encode_utf16(&mut buf);
                units.extend_from_slice(encoded);
                i += 1;
            }
        }
        JsString(units.into_boxed_slice())
    }

    // ── Access ──────────────────────────────────────────────────────────

    /// Returns the WTF-16 code units as a slice.
    #[inline]
    pub fn as_utf16(&self) -> &[u16] {
        &self.0
    }

    /// Returns the length in UTF-16 code units (= JS `.length`).
    ///
    /// §22.1.3.3 get String.prototype.length
    /// Spec: <https://tc39.es/ecma262/#sec-properties-of-string-instances-length>
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if the string is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the UTF-16 code unit at the given index, or `None` if out of bounds.
    ///
    /// §22.1.3.2 String.prototype.charCodeAt(pos)
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.charcodeat>
    #[inline]
    pub fn code_unit_at(&self, index: usize) -> Option<u16> {
        self.0.get(index).copied()
    }

    /// Returns the Unicode code point starting at the given UTF-16 index.
    ///
    /// If the code unit at `index` is the start of a surrogate pair, returns
    /// the combined code point and advances by 2. Otherwise returns the single
    /// code unit as a code point.
    ///
    /// §22.1.3.3 String.prototype.codePointAt(pos)
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.codepointat>
    pub fn code_point_at(&self, index: usize) -> Option<(u32, usize)> {
        let lead = *self.0.get(index)?;
        if (0xD800..=0xDBFF).contains(&lead)
            && let Some(&trail) = self.0.get(index + 1)
            && (0xDC00..=0xDFFF).contains(&trail)
        {
            let cp = 0x10000 + ((lead as u32 - 0xD800) << 10) + (trail as u32 - 0xDC00);
            return Some((cp, 2));
        }
        Some((lead as u32, 1))
    }

    // ── Conversion ─────────────────────────────────────────────────────

    /// Converts to a Rust `String`, replacing lone surrogates with U+FFFD.
    ///
    /// This is lossy for strings containing lone surrogates.
    #[inline]
    pub fn to_rust_string(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }

    /// Converts to a Rust `String` if the string is valid UTF-16.
    ///
    /// Returns `None` if the string contains lone surrogates.
    #[inline]
    pub fn to_rust_string_lossless(&self) -> Option<String> {
        String::from_utf16(&self.0).ok()
    }

    /// Returns `true` if this string is well-formed UTF-16 (no lone surrogates).
    ///
    /// §22.1.3.9 String.prototype.isWellFormed()
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.iswellformed>
    pub fn is_well_formed(&self) -> bool {
        let mut i = 0;
        while i < self.0.len() {
            let code = self.0[i];
            if (0xD800..=0xDBFF).contains(&code) {
                if i + 1 >= self.0.len() || !(0xDC00..=0xDFFF).contains(&self.0[i + 1]) {
                    return false;
                }
                i += 2;
            } else if (0xDC00..=0xDFFF).contains(&code) {
                return false;
            } else {
                i += 1;
            }
        }
        true
    }

    /// Returns a new string with lone surrogates replaced by U+FFFD.
    ///
    /// §22.1.3.33 String.prototype.toWellFormed()
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.towellformed>
    pub fn to_well_formed(&self) -> JsString {
        let mut result = Vec::with_capacity(self.0.len());
        let mut i = 0;
        while i < self.0.len() {
            let code = self.0[i];
            if (0xD800..=0xDBFF).contains(&code) {
                if i + 1 < self.0.len() && (0xDC00..=0xDFFF).contains(&self.0[i + 1]) {
                    result.push(code);
                    result.push(self.0[i + 1]);
                    i += 2;
                } else {
                    result.push(0xFFFD);
                    i += 1;
                }
            } else if (0xDC00..=0xDFFF).contains(&code) {
                result.push(0xFFFD);
                i += 1;
            } else {
                result.push(code);
                i += 1;
            }
        }
        JsString(result.into_boxed_slice())
    }

    // ── Substring / Slice ──────────────────────────────────────────────

    /// Returns a substring by UTF-16 code unit indices.
    ///
    /// §22.1.3.25 String.prototype.substring(start, end)
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.substring>
    pub fn substring(&self, start: usize, end: usize) -> JsString {
        let start = start.min(self.0.len());
        let end = end.min(self.0.len());
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        JsString(self.0[start..end].into())
    }

    /// Returns a slice by UTF-16 code unit range (for `String.prototype.slice`).
    pub fn slice(&self, start: usize, end: usize) -> JsString {
        if start >= end || start >= self.0.len() {
            return JsString::empty();
        }
        let end = end.min(self.0.len());
        JsString(self.0[start..end].into())
    }

    // ── Search ─────────────────────────────────────────────────────────

    /// Finds the first occurrence of `search` starting at `from_index`.
    ///
    /// Returns the UTF-16 code unit index, or `None`.
    ///
    /// §22.1.3.9 String.prototype.indexOf(searchString, position)
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.indexof>
    pub fn index_of(&self, search: &JsString, from_index: usize) -> Option<usize> {
        if search.0.is_empty() {
            return Some(from_index.min(self.0.len()));
        }
        if from_index + search.0.len() > self.0.len() {
            return None;
        }
        for i in from_index..=(self.0.len() - search.0.len()) {
            if self.0[i..i + search.0.len()] == *search.0 {
                return Some(i);
            }
        }
        None
    }

    /// Finds the last occurrence of `search` up to `from_index`.
    ///
    /// §22.1.3.10 String.prototype.lastIndexOf(searchString, position)
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.lastindexof>
    pub fn last_index_of(&self, search: &JsString, from_index: usize) -> Option<usize> {
        if search.0.is_empty() {
            return Some(from_index.min(self.0.len()));
        }
        if search.0.len() > self.0.len() {
            return None;
        }
        let max_start = from_index.min(self.0.len() - search.0.len());
        for i in (0..=max_start).rev() {
            if self.0[i..i + search.0.len()] == *search.0 {
                return Some(i);
            }
        }
        None
    }

    /// Returns `true` if `self` starts with `prefix`.
    pub fn starts_with(&self, prefix: &JsString) -> bool {
        self.0.starts_with(&prefix.0)
    }

    /// Returns `true` if `self` ends with `suffix`.
    pub fn ends_with(&self, suffix: &JsString) -> bool {
        self.0.ends_with(&suffix.0)
    }

    /// Returns `true` if `self` contains `search`.
    pub fn contains(&self, search: &JsString) -> bool {
        self.index_of(search, 0).is_some()
    }

    // ── Concatenation ──────────────────────────────────────────────────

    /// Concatenates two `JsString` values.
    pub fn concat(&self, other: &JsString) -> JsString {
        let mut units = Vec::with_capacity(self.0.len() + other.0.len());
        units.extend_from_slice(&self.0);
        units.extend_from_slice(&other.0);
        JsString(units.into_boxed_slice())
    }

    // ── Repeat ─────────────────────────────────────────────────────────

    /// Repeats the string `count` times.
    ///
    /// §22.1.3.17 String.prototype.repeat(count)
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.repeat>
    pub fn repeat(&self, count: usize) -> JsString {
        let mut units = Vec::with_capacity(self.0.len() * count);
        for _ in 0..count {
            units.extend_from_slice(&self.0);
        }
        JsString(units.into_boxed_slice())
    }

    // ── Case conversion (ASCII fast path) ──────────────────────────────

    /// Converts to lowercase (basic ASCII + Unicode via Rust's char::to_lowercase).
    pub fn to_lowercase(&self) -> JsString {
        let s = self.to_rust_string();
        JsString::from_str(&s.to_lowercase())
    }

    /// Converts to uppercase (basic ASCII + Unicode via Rust's char::to_uppercase).
    pub fn to_uppercase(&self) -> JsString {
        let s = self.to_rust_string();
        JsString::from_str(&s.to_uppercase())
    }

    // ── Trim ───────────────────────────────────────────────────────────

    /// Trims whitespace from both ends.
    pub fn trim(&self) -> JsString {
        let s = self.to_rust_string();
        JsString::from_str(s.trim())
    }

    /// Trims whitespace from the start.
    pub fn trim_start(&self) -> JsString {
        let s = self.to_rust_string();
        JsString::from_str(s.trim_start())
    }

    /// Trims whitespace from the end.
    pub fn trim_end(&self) -> JsString {
        let s = self.to_rust_string();
        JsString::from_str(s.trim_end())
    }
}

// ── Trait implementations ──────────────────────────────────────────────────

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl PartialEq<str> for JsString {
    fn eq(&self, other: &str) -> bool {
        let other_utf16: Vec<u16> = other.encode_utf16().collect();
        *self.0 == *other_utf16
    }
}

impl PartialEq<&str> for JsString {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl Hash for JsString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Debug for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JsString({:?})", self.to_rust_string())
    }
}

impl fmt::Display for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_rust_string())
    }
}

impl From<&str> for JsString {
    #[inline]
    fn from(s: &str) -> Self {
        JsString::from_str(s)
    }
}

impl From<String> for JsString {
    #[inline]
    fn from(s: String) -> Self {
        JsString::from_str(&s)
    }
}

impl From<Box<str>> for JsString {
    #[inline]
    fn from(s: Box<str>) -> Self {
        JsString::from_str(&s)
    }
}

impl From<Vec<u16>> for JsString {
    #[inline]
    fn from(units: Vec<u16>) -> Self {
        JsString::from_utf16_vec(units)
    }
}

impl From<Box<[u16]>> for JsString {
    #[inline]
    fn from(units: Box<[u16]>) -> Self {
        JsString(units)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_ascii() {
        let s = JsString::from_str("hello");
        assert_eq!(s.len(), 5);
        assert_eq!(s.as_utf16(), &[104, 101, 108, 108, 111]);
    }

    #[test]
    fn from_str_emoji() {
        // U+1F600 "😀" — surrogate pair D83D DE00
        let s = JsString::from_str("😀");
        assert_eq!(s.len(), 2); // Two UTF-16 code units
        assert_eq!(s.as_utf16(), &[0xD83D, 0xDE00]);
    }

    #[test]
    fn lone_surrogate_via_utf16() {
        // Create a string with a lone high surrogate
        let s = JsString::from_utf16(vec![0xD800]);
        assert_eq!(s.len(), 1);
        assert!(!s.is_well_formed());
    }

    #[test]
    fn is_well_formed_valid() {
        let s = JsString::from_str("hello 😀");
        assert!(s.is_well_formed());
    }

    #[test]
    fn is_well_formed_lone_high() {
        let s = JsString::from_utf16(vec![0xD800]);
        assert!(!s.is_well_formed());
    }

    #[test]
    fn is_well_formed_lone_low() {
        let s = JsString::from_utf16(vec![0xDC00]);
        assert!(!s.is_well_formed());
    }

    #[test]
    fn is_well_formed_reversed_pair() {
        let s = JsString::from_utf16(vec![0xDC00, 0xD800]);
        assert!(!s.is_well_formed());
    }

    #[test]
    fn is_well_formed_valid_pair() {
        let s = JsString::from_utf16(vec![0xD800, 0xDC00]);
        assert!(s.is_well_formed());
    }

    #[test]
    fn to_well_formed_replaces_lone() {
        let s = JsString::from_utf16(vec![0x61, 0xD800, 0x62]);
        let well = s.to_well_formed();
        assert_eq!(well.as_utf16(), &[0x61, 0xFFFD, 0x62]);
    }

    #[test]
    fn to_well_formed_preserves_valid() {
        let s = JsString::from_utf16(vec![0xD800, 0xDC00]);
        let well = s.to_well_formed();
        assert_eq!(well.as_utf16(), &[0xD800, 0xDC00]);
    }

    #[test]
    fn oxc_decode_lone_surrogate() {
        // oxc encodes \uD800 as "\u{FFFD}d800"
        let encoded = "\u{FFFD}d800";
        let s = JsString::from_oxc_encoded(encoded);
        assert_eq!(s.as_utf16(), &[0xD800]);
        assert!(!s.is_well_formed());
    }

    #[test]
    fn oxc_decode_literal_fffd() {
        // oxc encodes literal U+FFFD as "\u{FFFD}fffd"
        let encoded = "\u{FFFD}fffd";
        let s = JsString::from_oxc_encoded(encoded);
        assert_eq!(s.as_utf16(), &[0xFFFD]);
    }

    #[test]
    fn oxc_decode_mixed() {
        // "abc\uD800def" encoded by oxc as "abc\u{FFFD}d800def"
        let encoded = "abc\u{FFFD}d800def";
        let s = JsString::from_oxc_encoded(encoded);
        assert_eq!(s.len(), 7);
        assert_eq!(s.as_utf16()[3], 0xD800);
        assert!(!s.is_well_formed());
    }

    #[test]
    fn index_of_basic() {
        let s = JsString::from_str("hello world");
        let search = JsString::from_str("world");
        assert_eq!(s.index_of(&search, 0), Some(6));
    }

    #[test]
    fn last_index_of_basic() {
        let s = JsString::from_str("aba");
        let search = JsString::from_str("a");
        assert_eq!(s.last_index_of(&search, 3), Some(2));
    }

    #[test]
    fn equality() {
        let a = JsString::from_str("hello");
        let b = JsString::from_str("hello");
        assert_eq!(a, b);
    }

    #[test]
    fn equality_with_surrogates() {
        let a = JsString::from_utf16(vec![0xD800]);
        let b = JsString::from_utf16(vec![0xD800]);
        assert_eq!(a, b);
    }

    #[test]
    fn inequality_different_surrogates() {
        let a = JsString::from_utf16(vec![0xD800]);
        let b = JsString::from_utf16(vec![0xD801]);
        assert_ne!(a, b);
    }

    #[test]
    fn code_point_at_bmp() {
        let s = JsString::from_str("abc");
        assert_eq!(s.code_point_at(0), Some((0x61, 1)));
    }

    #[test]
    fn code_point_at_surrogate_pair() {
        let s = JsString::from_str("😀");
        assert_eq!(s.code_point_at(0), Some((0x1F600, 2)));
    }

    #[test]
    fn code_point_at_lone_surrogate() {
        let s = JsString::from_utf16(vec![0xD800]);
        assert_eq!(s.code_point_at(0), Some((0xD800, 1)));
    }
}
