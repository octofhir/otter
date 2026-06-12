//! Runtime regression coverage for TypedArray `toLocaleString` ordering.
//!
//! # Contents
//! - The initial iteration length is preserved when element conversion
//!   shrinks a resizable backing buffer.
//!
//! # Invariants
//! - Elements beyond the live view after shrink contribute empty
//!   strings while preserving separators.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-%typedarray%.prototype.tolocalestring>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<typed-array-to-locale-string-order>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn to_locale_string_keeps_initial_length_when_conversion_shrinks_buffer() {
    let completion = run(r#"
        const rab = new ArrayBuffer(4, { maxByteLength: 8 });
        const ta = new Int8Array(rab, 0, 4);
        let calls = 0;
        Number.prototype.toLocaleString = function() {
            calls++;
            if (calls === 2) rab.resize(2);
            return "n" + this.valueOf();
        };
        ta.toLocaleString();
        "#);
    assert_eq!(completion, "n0,n0,,");
}
