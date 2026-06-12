//! Runtime regression coverage for `AggregateError` constructor semantics.
//!
//! # Contents
//! - IterableToList materialization for the `errors` argument.
//! - Observable ordering between `message` coercion and error iteration.
//!
//! # Invariants
//! - The compiler must route `AggregateError(...)` through the real
//!   constructor instead of the legacy error opcode shortcut.
//! - The installed `errors` property contains a materialized Array and is
//!   non-enumerable.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-aggregate-error>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<aggregate-error-constructor>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn aggregate_error_materializes_errors_array() {
    let completion = run(r#"
        const err = new AggregateError(new Set(["a", "b"]), "msg");
        const desc = Object.getOwnPropertyDescriptor(err, "errors");
        Array.isArray(desc.value) + ":" + desc.value.join(",") + ":" +
            desc.enumerable + ":" + desc.writable + ":" + desc.configurable;
        "#);
    assert_eq!(completion, "true:a,b:false:true:true");
}

#[test]
fn aggregate_error_coerces_message_before_iterating_errors() {
    let completion = run(r#"
        const sequence = [];
        const message = {
            toString() {
                sequence.push("toString");
                return "";
            }
        };
        const errors = {
            [Symbol.iterator]() {
                sequence.push("iterator");
                return {
                    next() {
                        sequence.push("next");
                        return { done: true };
                    }
                };
            }
        };
        new AggregateError(errors, message);
        sequence.join(",");
        "#);
    assert_eq!(completion, "toString,iterator,next");
}
