//! AggregateError diagnostic coverage for
//! ENGINE_REFACTOR_EXECUTION_PLAN §P2.3.
//!
//! §20.5.7.1 AggregateError(errors, message [, options]) — the
//! constructor stamps `errors` as a non-enumerable own data
//! property containing an array of the input iterable's
//! contents. The runtime diagnostic surface materialises that
//! array into [`otter_runtime::Diagnostic::aggregated_errors`].

use otter_runtime::{Diagnostic, OtterError, Runtime, SourceInput};

fn run_throwing(source: &str) -> Diagnostic {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let err = runtime
        .run_script(SourceInput::from_javascript(source), "<aggregate-test>")
        .expect_err("script throws");
    match err {
        OtterError::Runtime { diagnostic } => *diagnostic,
        other => panic!("expected Runtime error, got {other:?}"),
    }
}

#[test]
fn aggregate_error_constructor_preserves_errors_array() {
    let source = r#"
        throw new AggregateError(
            [new Error("one"), new TypeError("two")],
            "outer",
        );
    "#;
    let diag = run_throwing(source);
    assert!(diag.message.contains("AggregateError"));
    assert_eq!(
        diag.aggregated_errors.len(),
        2,
        "expected 2 aggregated errors, got {} (diag = {diag:?})",
        diag.aggregated_errors.len()
    );
    assert!(
        diag.aggregated_errors[0].message.contains("Error")
            && diag.aggregated_errors[0].message.contains("one"),
        "first aggregated error: {:?}",
        diag.aggregated_errors[0].message
    );
    assert!(
        diag.aggregated_errors[1].message.contains("TypeError")
            && diag.aggregated_errors[1].message.contains("two"),
        "second aggregated error: {:?}",
        diag.aggregated_errors[1].message
    );
}

#[test]
fn aggregate_error_diagnostic_round_trips_through_json() {
    let source = r#"
        throw new AggregateError(
            [new RangeError("a"), new SyntaxError("b")],
            "outer",
            { cause: new Error("aggregate-cause") },
        );
    "#;
    let diag = run_throwing(source);
    assert_eq!(diag.aggregated_errors.len(), 2);
    let cause = diag.cause.as_ref().expect("aggregate carries cause");
    assert!(cause.message.contains("aggregate-cause"));

    let json = serde_json::to_string_pretty(&diag).expect("serialize aggregate diagnostic");
    let parsed: Diagnostic = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.aggregated_errors.len(), 2);
    assert!(
        parsed
            .cause
            .as_ref()
            .is_some_and(|c| c.message.contains("aggregate-cause"))
    );
    let re = serde_json::to_string_pretty(&parsed).expect("re-serialize");
    assert_eq!(json, re, "AggregateError JSON round-trip diverged");
}

#[test]
fn empty_errors_array_does_not_attach_aggregated_field() {
    let diag = run_throwing(r#"throw new AggregateError([], "empty");"#);
    assert!(
        diag.aggregated_errors.is_empty(),
        "empty errors array should leave aggregated_errors empty: {diag:?}"
    );
}
