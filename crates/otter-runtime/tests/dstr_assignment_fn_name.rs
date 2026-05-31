//! Runtime coverage for NamedEvaluation of anonymous-function defaults
//! in destructuring *assignment* targets (§13.15.5.5) — e.g. the
//! `for ({ fn = function(){} } of …)` head, where the default function
//! must be named after the bound identifier.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-runtime-semantics-keyeddestructuringassignmentevaluation>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<dstr-fn-name-test>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn object_shorthand_default_names_anonymous_function() {
    assert_eq!(
        run("var fn; for ({ fn = function(){} } of [{}]) {} fn.name;"),
        "fn"
    );
}

#[test]
fn object_shorthand_default_keeps_named_expression_name() {
    assert_eq!(
        run("var xFn; for ({ xFn = function x(){} } of [{}]) {} xFn.name;"),
        "x"
    );
}

#[test]
fn object_shorthand_default_names_arrow() {
    assert_eq!(run("var f; for ({ f = () => 1 } of [{}]) {} f.name;"), "f");
}

#[test]
fn object_property_default_names_anonymous_function() {
    assert_eq!(
        run("var x; for ({ k: x = function(){} } of [{}]) {} x.name;"),
        "x"
    );
}

#[test]
fn array_element_default_names_anonymous_function() {
    assert_eq!(
        run("var a; for ([ a = function(){} ] of [[]]) {} a.name;"),
        "a"
    );
}
