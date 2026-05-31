//! Runtime coverage for §7.4.9 IteratorClose on abrupt `for…of`
//! completions — `break`, labelled `continue`, and `return` must call
//! the iterator's `return` method as control leaves the loop.
//!
//! # Contents
//! - `break` closes the innermost iterator.
//! - A labelled `continue`/`break` that exits a `for…of` closes it.
//! - `return` from inside the loop closes the iterator.
//! - A `return` inside a nested function does NOT close the outer
//!   loop's iterator.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-iteratorclose>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<for-of-close-test>")
        .expect("script")
        .completion_string()
        .to_string()
}

const ITERABLE: &str = r#"
    var rc = 0;
    var it = {
        [Symbol.iterator]() {
            return {
                next() { return { done: false, value: 1 }; },
                return() { rc++; return {}; }
            };
        }
    };
"#;

#[test]
fn break_closes_iterator() {
    let out = run(&format!("{ITERABLE} for (var x of it) {{ break; }} String(rc);"));
    assert_eq!(out, "1");
}

#[test]
fn labelled_continue_closes_crossed_iterator() {
    let out = run(&format!(
        "{ITERABLE} L: do {{ for (var x of it) {{ continue L; }} }} while (false); String(rc);"
    ));
    assert_eq!(out, "1");
}

#[test]
fn labelled_break_closes_crossed_iterator() {
    let out = run(&format!(
        "{ITERABLE} L: for (var y of [0]) {{ for (var x of it) {{ break L; }} }} String(rc);"
    ));
    assert_eq!(out, "1");
}

#[test]
fn return_closes_iterator() {
    let out = run(&format!(
        "{ITERABLE} (function() {{ for (var x of it) {{ return; }} }})(); String(rc);"
    ));
    assert_eq!(out, "1");
}

#[test]
fn nested_function_return_does_not_close_outer_iterator() {
    // The nested function's `return` must not touch the enclosing
    // loop's iterator; only the `break` closes it (rc == 1).
    let out = run(&format!(
        "{ITERABLE} for (var x of it) {{ (function() {{ return; }})(); break; }} String(rc);"
    ));
    assert_eq!(out, "1");
}
