//! Lexical-declaration early errors: §13.3.1.1 (`let` is not a valid
//! `let`/`const` BoundName) and §14.7.5.1 (a `for`-head lexical binding
//! must not collide with a `var` in the loop body). Both are
//! compile-time SyntaxErrors.

use otter_runtime::{Runtime, SourceInput};

fn compile_err(source: &str) -> bool {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<lex-early>")
        .is_err()
}

fn runs_ok(source: &str) -> bool {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<lex-early>")
        .is_ok()
}

#[test]
fn let_named_let_is_error() {
    assert!(compile_err("let let = 1;"));
    assert!(compile_err("const let = 1;"));
    assert!(compile_err("for (let let of []) {}"));
}

#[test]
fn var_named_let_is_allowed_sloppy() {
    assert!(runs_ok("var let = 1; let;"));
}

#[test]
fn for_of_head_let_conflicting_with_body_var_is_error() {
    assert!(compile_err("for (let x of []) { var x; }"));
    assert!(compile_err("for (let x of []) { { var x; } }"));
}

#[test]
fn for_of_head_let_with_unrelated_body_var_is_allowed() {
    assert!(runs_ok("for (let x of [1]) { var y = x; }"));
}

#[test]
fn body_var_in_nested_function_is_not_a_conflict() {
    assert!(runs_ok("for (let x of [1]) { (function(){ var x; })(); }"));
}
