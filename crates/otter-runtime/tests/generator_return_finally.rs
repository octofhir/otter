//! §27.5.3.4 GeneratorResumeAbrupt(return) — a generator closed via
//! `.return()` (e.g. `for…of` `break`) must resume the suspended body
//! so its `finally` blocks run; a `finally` may override the result.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<gen-return-finally>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn for_of_break_runs_generator_finally() {
    assert_eq!(
        run(
            "var fc=0; function* g(){ try { yield; } finally { fc++; } } for (var x of g()) { break; } String(fc);"
        ),
        "1"
    );
}

#[test]
fn return_runs_generator_finally_and_completes() {
    assert_eq!(
        run(
            "var fc=0; function* g(){ try { yield; } finally { fc++; } } var it=g(); it.next(); var r=it.return(42); r.value + ',' + r.done + ',' + fc;"
        ),
        "42,true,1"
    );
}

#[test]
fn generator_finally_return_overrides() {
    assert_eq!(
        run(
            "function* g(){ try { yield 1; } finally { return 99; } } var it=g(); it.next(); var r=it.return(5); r.value + ',' + r.done;"
        ),
        "99,true"
    );
}

#[test]
fn return_without_finally_completes_immediately() {
    assert_eq!(
        run(
            "function* g(){ yield 1; yield 2; } var it=g(); it.next(); var r=it.return(7); r.value + ',' + r.done;"
        ),
        "7,true"
    );
}
