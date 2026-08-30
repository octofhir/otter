//! Case folding for the `i` flag.
//!
//! Case-insensitive matching compares a canonical form of both pattern and
//! subject code points. The form depends on the unicode flag (§22.2.2.9
//! Canonicalize):
//! - With `u`/`v`, Unicode Simple Case Folding ([`fold_unicode`], from UCD
//!   `CaseFolding.txt` via ICU4X).
//! - Without, the Unicode Default Case Conversion UPPERCASE mapping
//!   ([`canonicalize`]), restricted to mappings that stay one code unit and
//!   never land a non-ASCII code point on an ASCII one.
//!
//! # Contents
//! - [`canonicalize`] — non-unicode `i` canonical form.
//! - [`fold_unicode`] — Unicode Simple Case Folding (`i`+`u`/`v`).
//! - [`ascii_other_case`] — the opposite-case ASCII letter, used to widen class
//!   membership under `i`.
//!
//! # Invariants
//! - Both folds are idempotent.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-runtime-semantics-canonicalize-ch> (§22.2.2.9)

/// §22.2.2.9 Canonicalize for non-unicode `i`.
///
/// The code point's Unicode Default Case Conversion uppercase mapping,
/// except that a mapping which is not a single UTF-16 code unit (step 7/8)
/// or which would move a non-ASCII code point onto an ASCII one (step 9)
/// leaves the code point unchanged. `ß` therefore stays `ß` (its uppercase
/// is `SS`), `ſ` stays `ſ` (its uppercase `S` is ASCII), and `µ` / `μ` both
/// canonicalize to `Μ`.
#[must_use]
pub(crate) fn canonicalize(cp: u32) -> u32 {
    // ASCII fast path: an ASCII code point's uppercase mapping is a single
    // ASCII code unit, so steps 7-9 never fire.
    if cp < 0x80 {
        return if (0x61..=0x7A).contains(&cp) {
            cp - 0x20
        } else {
            cp
        };
    }
    let Some(ch) = char::from_u32(cp) else {
        return cp;
    };
    let mut upper = ch.to_uppercase();
    let Some(first) = upper.next() else {
        return cp;
    };
    if upper.next().is_some() {
        // Step 7 — the uppercase mapping is more than one code point.
        return cp;
    }
    let cu = first as u32;
    if cu > 0xFFFF {
        // Step 8 — the mapping is not a single UTF-16 code unit.
        return cp;
    }
    if cu < 128 {
        // Step 9 — `cp` is already known to be >= 128 here.
        return cp;
    }
    cu
}

/// Unicode Simple Case Folding of one code point (`i`+`u`/`v`).
///
/// Lone surrogates and non-scalar values fold to themselves.
#[must_use]
pub(crate) fn fold_unicode(cp: u32) -> u32 {
    match char::from_u32(cp) {
        Some(c) => icu_casemap::CaseMapper::new().simple_fold(c) as u32,
        None => cp,
    }
}

/// The opposite-case ASCII letter for `cp`, or `cp` if it is not an ASCII letter.
///
/// Used to test class membership under `i`: a subject code point matches a class
/// if either it or its opposite case is in the class.
#[must_use]
pub(crate) fn ascii_other_case(cp: u32) -> u32 {
    if (0x41..=0x5A).contains(&cp) {
        cp + 0x20
    } else if (0x61..=0x7A).contains(&cp) {
        cp - 0x20
    } else {
        cp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_is_idempotent() {
        for cp in 0u32..0x3000 {
            assert_eq!(canonicalize(canonicalize(cp)), canonicalize(cp));
        }
    }

    #[test]
    fn canonicalize_follows_the_uppercase_mapping() {
        // µ (MICRO SIGN) and μ (GREEK SMALL MU) share Μ.
        assert_eq!(canonicalize(0x00B5), 0x039C);
        assert_eq!(canonicalize(0x03BC), 0x039C);
        assert_eq!(canonicalize(0x039C), 0x039C);
        // ASCII folds up.
        assert_eq!(canonicalize(u32::from(b'a')), u32::from(b'A'));
        // ß uppercases to "SS" — more than one code point (step 7).
        assert_eq!(canonicalize(0x00DF), 0x00DF);
        // ſ uppercases to ASCII "S" (step 9).
        assert_eq!(canonicalize(0x017F), 0x017F);
        // KELVIN SIGN is already uppercase and stays distinct from "K".
        assert_eq!(canonicalize(0x212A), 0x212A);
        assert_eq!(canonicalize(u32::from(b'k')), u32::from(b'K'));
    }

    #[test]
    fn other_case_toggles_letters() {
        assert_eq!(ascii_other_case(b'A' as u32), b'a' as u32);
        assert_eq!(ascii_other_case(b'a' as u32), b'A' as u32);
        assert_eq!(ascii_other_case(b'5' as u32), b'5' as u32);
    }
}
