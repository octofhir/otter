//! Runtime regression coverage for legacy `arguments.callee.caller`.
//!
//! # Contents
//! - Unsupported Annex-B caller fallback for sloppy ordinary functions.
//!
//! # Invariants
//! - Otter may decline the stack-sensitive caller extension by returning
//!   `undefined`, but must not expose a non-callable placeholder.
//! - Strict / restricted-function poisoning remains owned by
//!   `%Function.prototype%` restricted accessors.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-addrestrictedfunctionproperties>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<arguments-callee-caller>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn unsupported_arguments_callee_caller_is_undefined() {
    let completion = run(r#"
        function outer() {
            return inner();
        }
        function inner() {
            return String(arguments.callee.caller === undefined);
        }
        outer();
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn conditional_legacy_caller_path_does_not_call_non_callable_placeholder() {
    let completion = run(r#"
        var called = false;
        function outer(flag) {
            if (flag === true) {
                called = true;
            } else {
                inner();
            }
        }
        function inner() {
            if (arguments.callee.caller === undefined) {
                called = true;
            } else {
                arguments.callee.caller(true);
            }
        }
        outer();
        String(called);
        "#);
    assert_eq!(completion, "true");
}
