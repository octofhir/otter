//! Case folding for the `i` flag.
//!
//! Case-insensitive matching compares a canonical form of both pattern and
//! subject code points. The form depends on the unicode flag (§22.2.2.7.1
//! Canonicalize):
//! - With `u`/`v`, Unicode Simple Case Folding ([`fold_unicode`], from UCD
//!   `CaseFolding.txt` via ICU4X).
//! - Without, the Basic-Latin (ASCII) fold ([`canonicalize`]) — ECMAScript's
//!   non-unicode `Canonicalize` never maps a non-ASCII code point onto an ASCII
//!   one, so the ASCII fold is exact for the letters it touches.
//!
//! # Contents
//! - [`canonicalize`] — ASCII case-fold form (non-unicode `i`).
//! - [`fold_unicode`] — Unicode Simple Case Folding (`i`+`u`/`v`).
//! - [`ascii_other_case`] — the opposite-case ASCII letter, used to widen class
//!   membership under `i`.
//!
//! # Invariants
//! - Both folds are idempotent.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-runtime-semantics-canonicalize-ch> (§22.2.2.7.1)

/// Canonical ASCII case-fold form of one code point (non-unicode `i`).
///
/// ASCII upper-case letters fold to lower-case; all other code points are
/// returned unchanged.
#[must_use]
pub(crate) fn canonicalize(cp: u32) -> u32 {
    if (0x41..=0x5A).contains(&cp) {
        cp + 0x20
    } else {
        cp
    }
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
        for cp in 0u32..0x80 {
            assert_eq!(canonicalize(canonicalize(cp)), canonicalize(cp));
        }
    }

    #[test]
    fn other_case_toggles_letters() {
        assert_eq!(ascii_other_case(b'A' as u32), b'a' as u32);
        assert_eq!(ascii_other_case(b'a' as u32), b'A' as u32);
        assert_eq!(ascii_other_case(b'5' as u32), b'5' as u32);
    }
}
