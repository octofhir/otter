//! `Error.cause` chain coverage for ENGINE_REFACTOR_EXECUTION_PLAN §P2.3.
//!
//! §20.5.6.1.1 InstallErrorCause: when `new Error(message, options)`
//! is called with `options` containing a `cause` property, the
//! resulting `Error` instance carries `cause` as a non-enumerable,
//! writable, configurable own data property. The runtime
//! diagnostic surface walks that chain and populates
//! [`otter_runtime::Diagnostic::cause`] recursively.

use otter_runtime::{Diagnostic, OtterError, Runtime, SourceInput};

fn run_throwing(source: &str) -> Diagnostic {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let err = runtime
        .run_script(SourceInput::from_javascript(source), "<cause-test>")
        .expect_err("script throws");
    match err {
        OtterError::Runtime { diagnostic } => diagnostic,
        other => panic!("expected Runtime error, got {other:?}"),
    }
}

#[test]
fn install_error_cause_attaches_cause_to_diagnostic() {
    let source = r#"
        throw new Error("outer", { cause: new TypeError("inner") });
    "#;
    let diag = run_throwing(source);
    let cause = diag.cause.as_ref().expect("cause chain present");
    assert!(
        cause.message.contains("TypeError") && cause.message.contains("inner"),
        "unexpected cause message: {:?}",
        cause.message
    );
}

#[test]
fn nested_cause_chain_round_trips_through_json() {
    let source = r#"
        const innermost = new RangeError("3");
        const middle = new TypeError("2", { cause: innermost });
        throw new Error("1", { cause: middle });
    "#;
    let diag = run_throwing(source);
    let first = diag.cause.as_ref().expect("first cause");
    let second = first.cause.as_ref().expect("second cause");
    assert!(
        second.message.contains("RangeError") && second.message.contains("3"),
        "innermost cause not surfaced: {:?}",
        second.message
    );

    let json = serde_json::to_string_pretty(&diag).expect("serialize");
    let parsed: Diagnostic = serde_json::from_str(&json).expect("deserialize");
    let re = serde_json::to_string_pretty(&parsed).expect("re-serialize");
    assert_eq!(json, re, "cause chain JSON round-trip diverged");
}

#[test]
fn options_without_cause_leaves_chain_empty() {
    // §20.5.6.1.1 step 4: if `options` lacks `cause`, no own
    // property is installed.
    let diag = run_throwing(r#"throw new Error("plain", { other: 1 });"#);
    assert!(
        diag.cause.is_none(),
        "diagnostic should have no cause: {:?}",
        diag.cause
    );
}

#[test]
fn non_object_thrown_value_has_no_cause() {
    let diag = run_throwing(r#"throw "plain string";"#);
    assert!(diag.cause.is_none());
    assert!(diag.aggregated_errors.is_empty());
}

#[test]
fn cause_chain_handles_self_reference_within_depth_limit() {
    // Pathological chain — both `e.cause` and `e.cause.cause`
    // point at the same object. The walker bails at the depth
    // guard rather than recursing forever.
    let source = r#"
        const e = new Error("self");
        e.cause = e;
        throw e;
    "#;
    let diag = run_throwing(source);
    assert!(
        diag.cause.is_some(),
        "self-cause chain should populate one cause"
    );
    // Walk the chain — should terminate without panicking.
    let mut current = diag.cause.as_deref();
    let mut depth = 0;
    while let Some(c) = current {
        depth += 1;
        if depth > 64 {
            panic!("cause chain did not terminate at depth limit");
        }
        current = c.cause.as_deref();
    }
}
