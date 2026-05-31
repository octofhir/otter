//! §13.7.x.1 — a `FunctionDeclaration` (possibly labelled) cannot be
//! the body of an iteration statement, in *both* strict and sloppy
//! modes (Annex B relaxes only the `if` arm). These must be a
//! compile-time SyntaxError.

use otter_runtime::{Runtime, SourceInput};

fn compile_err(source: &str) -> bool {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<iter-fn-body>")
        .is_err()
}

fn runs_ok(source: &str) -> bool {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<iter-fn-body>")
        .is_ok()
}

#[test]
fn for_of_function_body_is_error() {
    assert!(compile_err("for (var x of []) function f() {}"));
}

#[test]
fn for_of_labelled_function_body_is_error() {
    assert!(compile_err("for (var x of []) l1: l2: function f() {}"));
}

#[test]
fn while_function_body_is_error() {
    assert!(compile_err("while (false) function f() {}"));
}

#[test]
fn for_function_body_is_error() {
    assert!(compile_err("for (;;) function f() {}"));
}

#[test]
fn block_wrapped_function_body_is_allowed() {
    assert!(runs_ok("for (var x of [1]) { function f() { return 9; } }"));
}

#[test]
fn standalone_sloppy_labelled_function_is_allowed() {
    assert!(runs_ok("l: function f() {} f();"));
}
