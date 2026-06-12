//! Runtime regression coverage for the `Array.fromAsync` static surface.
//!
//! # Contents
//! - `Array.fromAsync` has the finalized builtin metadata shape.
//! - The builtin is callable but not constructible.
//! - Direct calls still report the intentionally missing async
//!   collection implementation.
//!
//! # Invariants
//! - Static Array methods are installed through the shared JS surface
//!   builder so `.name`, `.length`, property flags, extensibility, and
//!   `[[Construct]]` agree with other builtins.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-array.fromasync>

use otter_runtime::{OtterError, Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<array-from-async-test>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

fn run_throwing(source: &str) -> OtterError {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<array-from-async-test>",
    )
    .expect_err("script throws")
}

#[test]
fn array_from_async_has_builtin_metadata_shape() {
    let completion = run(r#"
        const desc = Object.getOwnPropertyDescriptor(Array, "fromAsync");
        const lengthDesc = Object.getOwnPropertyDescriptor(Array.fromAsync, "length");
        const nameDesc = Object.getOwnPropertyDescriptor(Array.fromAsync, "name");
        [
          typeof Array.fromAsync,
          desc.writable,
          desc.enumerable,
          desc.configurable,
          Object.isExtensible(Array.fromAsync),
          Object.getPrototypeOf(Array.fromAsync) === Function.prototype,
          String(Object.getOwnPropertyDescriptor(Array.fromAsync, "prototype")),
          Array.fromAsync.length,
          lengthDesc.writable,
          lengthDesc.enumerable,
          lengthDesc.configurable,
          Array.fromAsync.name,
          nameDesc.writable,
          nameDesc.enumerable,
          nameDesc.configurable
        ].join("|");
        "#);
    assert_eq!(
        completion,
        "function|true|false|true|true|true|undefined|1|false|false|true|fromAsync|false|false|true"
    );
}

#[test]
fn array_from_async_is_not_constructible() {
    let err = run_throwing("new Array.fromAsync();");
    let OtterError::Runtime { diagnostic } = err else {
        panic!("expected Runtime error, got {err:?}");
    };
    assert!(
        diagnostic.message.contains("TypeError"),
        "expected TypeError, got {:?}",
        diagnostic.message
    );
}

#[test]
fn array_from_async_direct_call_reports_missing_algorithm() {
    let err = run_throwing("Array.fromAsync([]);");
    let OtterError::Runtime { diagnostic } = err else {
        panic!("expected Runtime error, got {err:?}");
    };
    assert!(
        diagnostic
            .message
            .contains("Array.fromAsync collection is not implemented"),
        "expected not-implemented diagnostic, got {:?}",
        diagnostic.message
    );
}
