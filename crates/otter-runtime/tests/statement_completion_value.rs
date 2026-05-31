//! Runtime coverage for statement completion values (§13.x / §14.x
//! Runtime Semantics: Evaluation) as observed through `eval`.
//!
//! # Contents
//! - `if` / `while` / `do-while` / `for` / `for-of` yield their last
//!   non-empty body completion (or `undefined` when no branch / body
//!   runs).
//!
//! # Invariants
//! - A loop with an empty body or zero iterations completes with
//!   `undefined`; a non-empty body completion becomes the loop value.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-for-in-and-for-of-statements-runtime-semantics-labelledevaluation>

use otter_runtime::{Runtime, SourceInput};

fn eval(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<cptn-test>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn for_of_completion_value() {
    assert_eq!(eval("1; for (var a of [0]) { }"), "undefined");
    assert_eq!(eval("2; for (var b of [0]) { 3; }"), "3");
    assert_eq!(eval("4; for (var c of []) { 5; }"), "undefined");
}

#[test]
fn while_completion_value() {
    assert_eq!(eval("var i = 0; while (i < 1) { i++; 7; }"), "7");
    assert_eq!(eval("1; while (false) { 2; }"), "undefined");
}

#[test]
fn do_while_completion_value() {
    assert_eq!(eval("do { 8; } while (false)"), "8");
}

#[test]
fn for_completion_value() {
    assert_eq!(eval("for (var i = 0; i < 1; i++) { 9; }"), "9");
    assert_eq!(eval("1; for (var i = 0; i < 0; i++) { 2; }"), "undefined");
}

#[test]
fn if_completion_value() {
    assert_eq!(eval("if (true) { 5; }"), "5");
    assert_eq!(eval("if (false) { 5; }"), "undefined");
    assert_eq!(eval("if (false) { 5; } else { 6; }"), "6");
}
