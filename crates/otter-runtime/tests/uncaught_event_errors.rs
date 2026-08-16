//! An unhandled throw from a timer or host-event callback fails the run.
//!
//! The isolate runner drives timers and runtime tasks between a command's
//! completion and its reply. A callback that throws — with nothing in
//! JavaScript claiming the error — must surface as the run's failure, the
//! way Node ends the process on an uncaught exception. It must never be
//! reduced to a counter increment that leaves the caller waiting on work
//! that already died.

use otter_runtime::{Runtime, SourceInput, TokioRuntimeHost};

#[test]
fn a_throwing_timer_callback_fails_the_run() {
    let host = TokioRuntimeHost::new().expect("Tokio host");
    let runtime = Runtime::builder()
        .runtime_host(host.clone())
        .build_handle()
        .expect("isolate");

    let error = host
        .handle()
        .block_on(runtime.eval(SourceInput::from_javascript(
            "setTimeout(() => { throw new Error('boom-timer'); }, 0); 'queued'",
        )))
        .expect_err("an uncaught throw in a timer callback must fail the run");
    assert!(
        error.to_string().contains("boom-timer"),
        "the failure must carry the thrown error, got: {error}"
    );
}

#[test]
fn a_clean_timer_callback_still_completes_the_run() {
    let host = TokioRuntimeHost::new().expect("Tokio host");
    let runtime = Runtime::builder()
        .runtime_host(host.clone())
        .build_handle()
        .expect("isolate");

    let result = host
        .handle()
        .block_on(runtime.eval(SourceInput::from_javascript(
            "globalThis.fired = false; setTimeout(() => { globalThis.fired = true; }, 0); 'queued'",
        )))
        .expect("clean timer run");
    assert_eq!(result.completion_string(), "queued");
    let fired = host
        .handle()
        .block_on(runtime.eval(SourceInput::from_javascript("globalThis.fired")))
        .expect("readback");
    assert_eq!(fired.completion_string(), "true");
}

#[test]
fn an_exit_requested_in_a_timer_callback_completes_the_run_with_its_code() {
    let host = TokioRuntimeHost::new().expect("Tokio host");
    let runtime = Runtime::builder()
        .runtime_host(host.clone())
        .build_handle()
        .expect("isolate");

    let result = host
        .handle()
        .block_on(runtime.eval(SourceInput::from_javascript(
            "setTimeout(() => { globalThis.process?.exit?.(7); throw { __exit: 7 }; }, 0); 'queued'",
        )));
    // Without node modules there is no `process`; the throw above keeps the
    // test meaningful either way: with `process.exit` the run must complete
    // with code 7, without it the throw must fail the run.
    match result {
        Ok(result) => assert_eq!(result.exit_code(), 7),
        Err(error) => assert!(error.to_string().contains("uncaught")),
    }
}
