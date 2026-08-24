//! Runtime-handle process finalization tears down JavaScript timers.
//!
//! # Contents
//! - Normal completion with an unreferenced deadline.
//! - Explicit process exit with referenced and unreferenced deadlines.
//! - Uncaught interval failure followed by a fresh command.
//! - Command timeout cleanup.
//!
//! # Invariants
//! - No VM callback root, driver deadline, or timer-liveness hold crosses a
//!   finalized process boundary.
//! - A later command on the same isolate cannot revive an earlier command's
//!   timeout or interval.
//!
//! # See also
//! - `otter_vm::TimerCallbacks`
//! - `otter_runtime::Runtime::cancel_all_timers`

use std::time::Duration;

use otter_runtime::{Otter, OtterError};

fn assert_timer_baseline(otter: &Otter, cancelled: u64) {
    let stats = otter.activity_stats();
    assert_eq!(stats.pending_ref_timers, 0);
    assert_eq!(stats.pending_unref_timers, 0);
    assert_eq!(stats.cancelled_timers, cancelled);
}

async fn wait_for_timer_baseline(otter: &Otter, cancelled: u64) {
    for _ in 0..200 {
        let stats = otter.activity_stats();
        if !stats.running_command
            && stats.pending_ref_timers == 0
            && stats.pending_unref_timers == 0
            && stats.cancelled_timers == cancelled
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!otter.activity_stats().running_command);
    assert_timer_baseline(otter, cancelled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_completion_cancels_a_long_unref_timer() {
    let otter = Otter::new();
    let result = otter
        .run_script(
            r#"
            globalThis.normalTimerFired = 0;
            const timer = setTimeout(() => { normalTimerFired += 1; }, 60000);
            if (!__otterTimerSetRef(timer, false)) throw new Error("unref failed");
            "done";
            "#,
        )
        .await
        .expect("unref timer must not retain the command");
    assert_eq!(result.completion_string(), "done");
    assert_timer_baseline(&otter, 1);

    let later = otter
        .run_script("normalTimerFired")
        .await
        .expect("same isolate remains usable");
    assert_eq!(later.completion_string(), "0");
    assert_timer_baseline(&otter, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_exit_cancels_ref_unref_and_exit_listener_timers() {
    let otter = Otter::new();
    let result = otter
        .run_script(
            r#"
            globalThis.exitTimerFired = 0;
            setTimeout(() => { exitTimerFired += 1; }, 60000);
            const unrefed = setTimeout(() => { exitTimerFired += 10; }, 60000);
            if (!__otterTimerSetRef(unrefed, false)) throw new Error("unref failed");
            process.on("exit", () => {
                setTimeout(() => { exitTimerFired += 100; }, 60000);
            });
            process.exit(7);
            "#,
        )
        .await
        .expect("process.exit is a successful explicit completion");
    assert_eq!(result.exit_code(), 7);
    assert_timer_baseline(&otter, 3);

    let later = otter
        .run_script("exitTimerFired")
        .await
        .expect("same isolate remains usable");
    assert_eq!(later.completion_string(), "0");
    assert_timer_baseline(&otter, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncaught_interval_is_disarmed_before_the_next_command() {
    let otter = Otter::new();
    let error = otter
        .run_script(
            r#"
            globalThis.failedIntervalTicks = 0;
            setInterval(() => {
                failedIntervalTicks += 1;
                throw new Error("interval teardown boom");
            }, 1);
            "#,
        )
        .await
        .expect_err("an uncaught interval failure ends the command");
    assert!(
        format!("{error:?}").contains("interval teardown boom"),
        "unexpected error: {error:?}"
    );
    assert_timer_baseline(&otter, 1);

    tokio::time::sleep(Duration::from_millis(20)).await;
    let later = otter
        .run_script("failedIntervalTicks")
        .await
        .expect("same isolate remains usable");
    assert_eq!(later.completion_string(), "1");
    assert_timer_baseline(&otter, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_releases_every_timer_hold() {
    let otter = Otter::builder()
        .timeout(Duration::from_millis(20))
        .build()
        .expect("runtime");
    let error = otter
        .run_script(
            r#"
            globalThis.timeoutTimerFired = 0;
            setTimeout(() => { timeoutTimerFired += 1; }, 60000);
            while (true) {}
            "#,
        )
        .await
        .expect_err("command must time out");
    assert!(matches!(error, OtterError::Timeout { .. }));
    // The public timeout returns as soon as it publishes the cooperative
    // interrupt. Wait for the isolate thread to finish its teardown before
    // observing the accounting boundary.
    wait_for_timer_baseline(&otter, 1).await;

    let later = otter
        .run_script("timeoutTimerFired")
        .await
        .expect("same isolate remains usable");
    assert_eq!(later.completion_string(), "0");
    assert_timer_baseline(&otter, 1);
}
