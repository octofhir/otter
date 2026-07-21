//! `Runtime::eval_value` hands the completion back as a live VM value.
//!
//! `ExecutionResult` renders the completion to a `String`, which is enough
//! for a CLI and useless to an embedder that wants the object back.

use otter_runtime::{Runtime, SourceInput};

fn runtime() -> Runtime {
    Runtime::builder().build().expect("runtime builds")
}

#[test]
fn returns_the_completion_value_rather_than_its_rendering() {
    let mut runtime = runtime();
    let doubled = runtime
        .eval_value(
            SourceInput::from_javascript("40 + 2"),
            "<test>",
            |_ctx, value| value.as_f64().expect("a number completion") * 2.0,
        )
        .expect("script runs");
    assert_eq!(doubled, 84.0);
}

#[test]
fn the_completion_object_is_readable_through_the_native_context() {
    let mut runtime = runtime();
    let port = runtime
        .eval_value(
            SourceInput::from_javascript("({ host: 'localhost', port: 8080 })"),
            "<test>",
            |ctx, value| {
                ctx.scope(|mut scope| {
                    let object = scope.value(value);
                    let port = scope.get(object, "port").expect("port property");
                    scope.number_value(port).ok()
                })
            },
        )
        .expect("script runs");
    assert_eq!(port, Some(8080.0));
}

#[test]
fn the_completion_survives_the_microtask_checkpoint() {
    let mut runtime = runtime();
    // The checkpoint allocates, and the young generation is a moving
    // collector: an unrooted completion would be laundered by the scavenge
    // this script forces before `with_value` ever runs.
    let value = runtime
        .eval_value(
            SourceInput::from_javascript(
                "const result = { marker: 4242 };
                 queueMicrotask(() => { const churn = []; for (let i = 0; i < 20000; i++) { churn.push({ i }); } });
                 result",
            ),
            "<test>",
            |ctx, value| {
                ctx.scope(|mut scope| {
                    let object = scope.value(value);
                    let marker = scope.get(object, "marker").expect("marker property");
                    scope.number_value(marker).ok()
                })
            },
        )
        .expect("script runs");
    assert_eq!(value, Some(4242.0));
}

#[test]
fn shares_globalthis_with_the_other_script_entry_points() {
    let mut runtime = runtime();
    runtime
        .eval(SourceInput::from_javascript("globalThis.seeded = 7"))
        .expect("seed runs");
    let seen = runtime
        .eval_value(
            SourceInput::from_javascript("globalThis.seeded"),
            "<test>",
            |_ctx, value| value.as_f64(),
        )
        .expect("script runs");
    assert_eq!(seen, Some(7.0));
}

#[test]
fn a_throwing_script_reports_the_error_and_leaves_the_runtime_usable() {
    let mut runtime = runtime();
    let error = runtime
        .eval_value(
            SourceInput::from_javascript("throw new TypeError('boom')"),
            "<test>",
            |_ctx, _value| unreachable!("the closure must not run for a failed script"),
        )
        .expect_err("script throws");
    assert!(
        format!("{error:?}").contains("boom"),
        "the diagnostic carries the throw: {error:?}"
    );

    let recovered = runtime
        .eval_value(
            SourceInput::from_javascript("1 + 1"),
            "<test>",
            |_ctx, v| v.as_f64(),
        )
        .expect("the isolate stays usable");
    assert_eq!(recovered, Some(2.0));
}
