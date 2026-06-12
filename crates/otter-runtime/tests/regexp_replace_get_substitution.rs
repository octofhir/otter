//! Runtime regression coverage for RegExp `GetSubstitution`.
//!
//! # Contents
//! - `$<` remains literal when a regexp has no named captures.
//!
//! # Invariants
//! - Named replacement parsing depends on the match result's `groups` value;
//!   without named captures only `$<` is consumed as literal text and the rest
//!   of the template is still scanned for numbered substitutions.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-getsubstitution>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<regexp-replace-get-substitution>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn replacement_keeps_named_marker_literal_without_named_captures() {
    let completion = run(r#"
        let re = /(.)(.)|(x)/;
        [
            "abcd".replace(re, "$<snd>$<fst>"),
            "abcd".replace(re, "$<42$1>"),
            "abcd".replace(re, "$<$1>"),
        ].join("|");
        "#);
    assert_eq!(completion, "$<snd>$<fst>cd|$<42a>cd|$<a>cd");
}
