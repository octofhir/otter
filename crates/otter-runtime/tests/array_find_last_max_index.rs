//! Runtime regression coverage for `Array.prototype.findLast*` on
//! pathological array-like lengths.
//!
//! # Contents
//! - `findLast` visits the clamped maximum valid index.
//! - `findLastIndex` reports the same clamped maximum valid index.
//!
//! # Invariants
//! - `LengthOfArrayLike` clamps to `2**53 - 1`, but the generic
//!   callback driver must not materialize or probe that full range.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-array.prototype.findlast>
//! - <https://tc39.es/ecma262/#sec-array.prototype.findlastindex>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<array-find-last-max-index>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn find_last_visits_clamped_maximum_index() {
    let completion = run(r#"
        const seen = [];
        Array.prototype.findLast.call({ length: Number.MAX_VALUE }, function (_value, index) {
          seen.push(index);
          return true;
        });
        seen.join(",");
        "#);
    assert_eq!(completion, "9007199254740990");
}

#[test]
fn find_last_index_reports_clamped_maximum_index() {
    let completion = run(r#"
        Array.prototype.findLastIndex.call({ length: Number.MAX_VALUE }, function () {
          return true;
        });
        "#);
    assert_eq!(completion, "9007199254740990");
}
