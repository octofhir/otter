//! An embedder calls a kept JavaScript function as one turn, without
//! compiling source.
//!
//! # Invariants
//!
//! - A call returns the function's completion and takes the microtask
//!   checkpoint afterwards, as a `run_script` of the same call would.
//! - A call links no code.
//! - An exception nothing catches is the call's error; the checkpoint still
//!   runs.
//! - An interrupt stops a call.

use otter_runtime::{CallArgument, Runtime, SourceInput};

fn runtime_with(source: &str) -> Runtime {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(SourceInput::from_javascript(source), "file:///call.js")
        .expect("setup runs");
    runtime
}

#[test]
fn a_call_returns_the_completion_and_drains_microtasks() {
    let mut runtime = runtime_with(
        r#"
var log = [];
function dispatch(kind, n, flag, text, nothing, missing) {
  Promise.resolve().then(() => log.push('microtask ' + kind));
  log.push([kind, n, flag, text, nothing, missing].join(':'));
  return n * 2;
}
function read() { return log.join('|'); }
"#,
    );
    let dispatch = runtime.global_function("dispatch").expect("a function");
    let read = runtime.global_function("read").expect("a function");
    let result = runtime
        .call(
            dispatch,
            &[
                CallArgument::String("click"),
                CallArgument::Number(21.0),
                CallArgument::Bool(true),
                CallArgument::String("x"),
                CallArgument::Null,
                CallArgument::Undefined,
            ],
        )
        .expect("a call");
    assert_eq!(result.completion_string(), "42");
    let log = runtime.call(read, &[]).expect("a call");
    assert_eq!(log.completion_string(), "click:21:true:x::|microtask click");
    runtime.release_function(dispatch);
    runtime.release_function(read);
}

#[test]
fn a_call_links_no_code() {
    let mut runtime = runtime_with("var n = 0; function tick() { return ++n; }");
    let tick = runtime.global_function("tick").expect("a function");
    runtime.call(tick, &[]).expect("a call");
    let chunks = runtime.linked_code_chunks();
    for _ in 0..100 {
        runtime.call(tick, &[]).expect("a call");
    }
    assert_eq!(runtime.linked_code_chunks(), chunks);
    assert_eq!(
        runtime.call(tick, &[]).expect("a call").completion_string(),
        "102"
    );
    // The same turn through source links a chunk every time.
    runtime
        .run_script(SourceInput::from_javascript("tick()"), "<turn>")
        .expect("a turn");
    assert!(runtime.linked_code_chunks() > chunks);
}

#[test]
fn an_uncaught_exception_is_the_calls_error_after_the_checkpoint() {
    let mut runtime = runtime_with(
        r#"
var settled = false;
function fail() { Promise.resolve().then(() => { settled = true; }); throw new TypeError('nope'); }
function wasSettled() { return settled; }
"#,
    );
    let fail = runtime.global_function("fail").expect("a function");
    let error = runtime.call(fail, &[]).expect_err("a throw");
    assert!(error.to_string().contains("nope"), "{error}");
    let was_settled = runtime.global_function("wasSettled").expect("a function");
    assert_eq!(
        runtime
            .call(was_settled, &[])
            .expect("a call")
            .completion_string(),
        "true"
    );
}

#[test]
fn a_name_that_is_not_a_callable_global_is_refused() {
    let mut runtime = runtime_with("var notAFunction = 1;");
    assert!(runtime.global_function("notAFunction").is_err());
    assert!(runtime.global_function("missing").is_err());
}

#[test]
fn an_interrupt_stops_a_call() {
    let mut runtime = runtime_with("function spin() { for (;;) {} }");
    let spin = runtime.global_function("spin").expect("a function");
    let interrupt = runtime.interrupt_handle();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        interrupt.interrupt();
    });
    assert!(runtime.call(spin, &[]).is_err());
    stopper.join().expect("stopper");
}
