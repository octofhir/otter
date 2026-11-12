//! Public parser-only boundary, also checked against runtime compilation.
//!
//! # Contents
//! - UTF-16, Unicode sets, named groups and early-error acceptance.
//!
//! # Invariants
//! - Validation and compilation return identical errors, including offsets.
//! - The same acceptance cases run with and without the runtime feature.

use otter_regex::{Flags, validate_utf16};

#[test]
fn validation_preserves_runtime_acceptance_and_diagnostics() {
    for (pattern, flags, valid) in [
        ("", "", true),
        (r"(?<word>a+)\k<word>", "u", true),
        (r"(?<x>a)|(?<x>b)", "u", true),
        (r"(?<x>a)(?<x>b)", "u", false),
        (r"(?im-s:^a.$)", "", true),
        #[cfg(feature = "unicode")]
        (r"[\p{ASCII}&&[^A-Z]]", "v", true),
        (r"[\q{ab|cd}]", "v", true),
        (r"[^\q{ab}]", "v", false),
        (r"\p{Not_A_Property}", "u", false),
        (r"\a", "", true),
        (r"\a", "u", false),
        (r"a{4,2}", "", false),
        (r"[z-a]", "", false),
        (r"(abc", "", false),
        (r"\k<missing>", "u", false),
        (r"[a-z]{100}", "i", true),
    ] {
        let units: Vec<u16> = pattern.encode_utf16().collect();
        let flags = Flags::from_str_lossy(flags);
        let result = validate_utf16(&units, flags);
        assert_eq!(result.is_ok(), valid, "{pattern}: {result:?}");
        #[cfg(feature = "runtime")]
        assert_eq!(
            result,
            otter_regex::Regex::compile_utf16(&units, flags).map(|_| ()),
            "{pattern}"
        );
    }
    for units in [vec![0xd800], vec![0xdc00], vec![0xd83d, 0xde00]] {
        assert_eq!(validate_utf16(&units, Flags::default()), Ok(()));
        #[cfg(feature = "runtime")]
        assert!(otter_regex::Regex::compile_utf16(&units, Flags::default()).is_ok());
    }
}
