//! Runtime regression coverage for `Array.from` constructor dispatch.
//!
//! # Contents
//! - Iterable-source target construction through a custom `this` value.
//!
//! # Invariants
//! - Array fast paths preserve §23.1.2.1 iterator-path construction:
//!   `Construct(C)` receives no length argument.
//! - Array-like, non-iterable sources remain free to forward the final
//!   length through `Construct(C, «len»)` in the separate path.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-array.from>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<array-from-constructor>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn array_fast_path_constructs_custom_target_without_length_argument() {
    let completion = run(r#"
        Array.from.call(Object, []).constructor === Object;
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn array_fast_path_still_populates_elements_and_length() {
    let completion = run(r#"
        const out = Array.from.call(Object, [3, 4]);
        out.constructor === Object && out.length === 2 && out[0] === 3 && out[1] === 4;
        "#);
    assert_eq!(completion, "true");
}
