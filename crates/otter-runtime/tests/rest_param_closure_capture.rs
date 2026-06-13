//! Regression: a function's REST parameter must be capturable by a nested
//! closure, exactly like an ordinary named parameter or a local binding.
//!
//! The capture pre-pass collects the names a function declares at its own depth
//! so they can be promoted to heap upvalue cells when an inner function reads
//! them. The rest element lives in `FormalParameters.rest` (not `items`), so it
//! was missed — a nested closure referencing it resolved to `undefined` and
//! threw `ReferenceError`. This pins the fix.

use otter_runtime::{Runtime, SourceInput};

#[test]
fn rest_parameter_captured_by_nested_closure() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let result = rt
        .run_script(
            SourceInput::from_javascript(
                "(function f(...args) { return [1, 2].map((x) => args[0] + x).join(','); })(10)",
            ),
            "<rest-capture>",
        )
        .expect("script ran");
    assert_eq!(result.completion_string(), "11,12");
}

#[test]
fn rest_parameter_captured_by_arrow() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let result = rt
        .run_script(
            SourceInput::from_javascript(
                "((...args) => [0, 1, 2].map((i) => args[i]).join('-'))('a', 'b', 'c')",
            ),
            "<rest-capture-arrow>",
        )
        .expect("script ran");
    assert_eq!(result.completion_string(), "a-b-c");
}
