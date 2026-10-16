//! A class constructor's `return f()` runs `f` once, on every tier.
//!
//! A class constructor is only ever run by `[[Construct]]`, which owes the
//! constructor completion after the body (§10.2.2), so no call in it is a
//! proper tail call. Compiled as one, the optimizing tier ran the call a
//! second time once the constructor was hot: every side effect of `f`
//! happened twice. Custom elements hit it through the `HTMLElement`
//! constructor (`return htmlElementConstructor(new.target)`).

use otter_runtime::{JitSelection, Runtime, SourceInput};

fn calls_after(selection: JitSelection, body: &str) -> String {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(format!(
                "globalThis.calls = 0;\
                 const make = () => {{ calls++; return {{}}; }};\
                 const one = () => {{ calls++; return 1; }};\
                 {body}"
            )),
            "https://site.test/run.js",
        )
        .expect("script runs");
    runtime
        .run_script(
            SourceInput::from_javascript("String(calls)"),
            "https://site.test/read.js",
        )
        .expect("count reads")
        .completion_string()
        .to_owned()
}

#[test]
fn a_returned_call_in_a_class_constructor_runs_once() {
    let cases = [
        "class D { constructor() { return make(); } } for (let i = 0; i < 5000; i++) new D();",
        "class D { constructor() { return one(); } } for (let i = 0; i < 5000; i++) new D();",
        "class B {} class D extends B { constructor() { return make(); } } for (let i = 0; i < 5000; i++) Reflect.construct(D, []);",
        "class D { constructor() { return make(); } } function build() { return new D(); } for (let i = 0; i < 5000; i++) build();",
    ];
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        for body in cases {
            assert_eq!(
                calls_after(selection, body),
                "5000",
                "{selection:?}: {body}"
            );
        }
    }
}

#[test]
fn a_strict_function_keeps_its_tail_calls() {
    // Not a constructor: the tail call is still a proper one, and a deep
    // chain of them runs in constant stack.
    let mut runtime = Runtime::builder().build().expect("runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                "'use strict'; function down(n) { if (n === 0) return 'done'; return down(n - 1); } down(200000);",
            ),
            "https://site.test/tail.js",
        )
        .expect("a deep tail chain runs");
    assert_eq!(result.completion_string(), "done");
}
