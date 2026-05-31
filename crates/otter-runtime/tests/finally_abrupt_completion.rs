//! §14.15.3 — `return` / `break` / `continue` must run the `finally`
//! blocks they cross before reaching their target, and a `finally`
//! that completes abruptly overrides the in-flight completion.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<finally-abrupt>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn return_runs_finally() {
    assert_eq!(
        run(
            "var f = 0; function g(){ try { return 1; } finally { f = 9; } } var r = g(); r + ',' + f;"
        ),
        "1,9"
    );
}

#[test]
fn finally_return_overrides() {
    assert_eq!(
        run("function g(){ try { return 1; } finally { return 2; } } String(g());"),
        "2"
    );
}

#[test]
fn nested_finally_runs_inner_then_outer_on_return() {
    assert_eq!(
        run(
            "var o=[]; function g(){ try { try { return 1; } finally { o.push('i'); } } finally { o.push('o'); } } var r=g(); r+':'+o.join(',');"
        ),
        "1:i,o"
    );
}

#[test]
fn break_runs_finally() {
    assert_eq!(
        run("var f=0; for (var i=0;i<3;i++){ try { break; } finally { f += 1; } } String(f);"),
        "1"
    );
}

#[test]
fn continue_runs_finally_each_iteration() {
    assert_eq!(
        run("var c=0; for (var i=0;i<3;i++){ try { continue; } finally { c += 1; } } String(c);"),
        "3"
    );
}

#[test]
fn labelled_break_runs_each_crossed_finally_inner_first() {
    assert_eq!(
        run(
            "var o=[]; L: for (var i=0;i<2;i++){ try { try { break L; } finally { o.push('a'); } } finally { o.push('b'); } } o.join(',');"
        ),
        "a,b"
    );
}

#[test]
fn async_return_runs_finally() {
    // The async frame settles its promise with the returned value
    // after the finally side effect runs.
    assert_eq!(
        run(
            "var f=0; var out='?'; async function g(){ try { return 5; } finally { f = 1; } } g().then(v => { out = v + ',' + f; }); out;"
        ),
        "?"
    );
}

#[test]
fn break_without_finally_is_unaffected() {
    assert_eq!(
        run("var n=0; for (var i=0;i<5;i++){ if (i===2) break; n++; } String(n);"),
        "2"
    );
}
