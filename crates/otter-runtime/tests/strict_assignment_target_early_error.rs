//! §12.7.1 — `eval` / `arguments` are invalid simple assignment
//! targets in strict code, including destructuring-assignment targets
//! and `for`-head targets that are not `AssignmentExpression` nodes.

use otter_runtime::{Runtime, SourceInput};

fn compile_err(source: &str) -> bool {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<strict-at>")
        .is_err()
}

fn runs_ok(source: &str) -> bool {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<strict-at>")
        .is_ok()
}

#[test]
fn strict_for_of_object_shorthand_eval_target_is_error() {
    assert!(compile_err("'use strict'; for ({ eval } of [{}]) ;"));
    assert!(compile_err("'use strict'; for ({ eval = 0 } of [{}]) ;"));
}

#[test]
fn strict_for_of_array_arguments_target_is_error() {
    assert!(compile_err("'use strict'; for ([arguments] of [[]]) ;"));
}

#[test]
fn strict_destructuring_assignment_eval_target_is_error() {
    assert!(compile_err("'use strict'; var o = {}; ({ eval } = o);"));
}

#[test]
fn sloppy_eval_target_is_allowed() {
    assert!(runs_ok("for ({ eval } of [{}]) ;"));
}
