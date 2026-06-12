//! Runtime regression coverage for `String.prototype.localeCompare`.
//!
//! # Contents
//! - Canonically equivalent Unicode strings compare equal.
//!
//! # Invariants
//! - The non-Intl fallback still preserves `ToString` behavior, but compares
//!   NFC-normalized UTF-16 units so canonical equivalents return zero.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-string.prototype.localecompare>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<string-locale-compare>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn locale_compare_treats_canonically_equivalent_strings_as_equal() {
    let completion = run(r#"
        [
            "o\u0308".localeCompare("\u00f6"),
            "\u212b".localeCompare("A\u030a"),
            "\u03a9".localeCompare("\u2126"),
        ].join("|");
        "#);
    assert_eq!(completion, "0|0|0");
}
