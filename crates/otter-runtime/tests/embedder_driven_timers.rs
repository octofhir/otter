//! Layer A embedders drive their own event loop, so they own the clock.
//!
//! This exercises the full host-scheduler contract without Layer B: install
//! a [`TimerScheduler`], let script call `setTimeout`, then deliver the fire
//! through [`Runtime::fire_timer`] the way a GUI or server loop would when
//! its own deadline elapses.

use std::sync::{Arc, Mutex};

use otter_runtime::{Runtime, SourceInput, TimerFireOutcome, TimerScheduler};

/// Records what the VM asked for instead of owning a real clock. A GUI
/// embedder would arm a timer wheel here and call `fire_timer` from its
/// loop; the test calls it directly so the ordering is deterministic.
#[derive(Debug, Default)]
struct RecordingScheduler {
    scheduled: Mutex<Vec<(u64, u64, Option<u64>)>>,
    cancelled: Mutex<Vec<u64>>,
    next_token: Mutex<u64>,
}

impl RecordingScheduler {
    fn tokens(&self) -> Vec<u64> {
        self.scheduled
            .lock()
            .expect("scheduled")
            .iter()
            .map(|(token, _, _)| *token)
            .collect()
    }
}

impl TimerScheduler for RecordingScheduler {
    fn schedule(&self, delay_ms: u64, repeat_ms: Option<u64>) -> u64 {
        let mut next = self.next_token.lock().expect("next_token");
        *next += 1;
        let token = *next;
        self.scheduled
            .lock()
            .expect("scheduled")
            .push((token, delay_ms, repeat_ms));
        token
    }

    fn cancel(&self, token: u64) -> bool {
        self.cancelled.lock().expect("cancelled").push(token);
        true
    }
}

fn runtime_with(scheduler: &Arc<RecordingScheduler>) -> Runtime {
    let mut runtime = Runtime::builder().build().expect("runtime builds");
    runtime.install_timer_scheduler(scheduler.clone());
    runtime
}

fn eval(runtime: &mut Runtime, source: &str) -> String {
    runtime
        .eval(SourceInput::from_javascript(source))
        .expect("script runs")
        .completion_string()
        .to_string()
}

#[test]
fn set_timeout_reaches_the_host_scheduler_and_fires_through_the_runtime() {
    let scheduler = Arc::new(RecordingScheduler::default());
    let mut runtime = runtime_with(&scheduler);

    eval(
        &mut runtime,
        "globalThis.fired = false; setTimeout(() => { globalThis.fired = true; }, 5);",
    );

    let scheduled = scheduler.scheduled.lock().expect("scheduled").clone();
    assert_eq!(scheduled.len(), 1, "setTimeout reaches the host scheduler");
    let (token, delay_ms, repeat_ms) = scheduled[0];
    assert_eq!(delay_ms, 5);
    assert_eq!(repeat_ms, None, "setTimeout is one-shot");

    assert!(
        runtime.has_pending_work(),
        "a live timer entry counts as pending work"
    );
    assert_eq!(eval(&mut runtime, "globalThis.fired"), "false");

    assert_eq!(
        runtime.fire_timer(token).expect("timer fires"),
        TimerFireOutcome::Fired { repeat: false }
    );

    assert_eq!(eval(&mut runtime, "globalThis.fired"), "true");
    assert!(
        !runtime.has_pending_work(),
        "the one-shot entry is gone once it has fired"
    );
}

#[test]
fn firing_a_one_shot_twice_reports_missing_rather_than_running_again() {
    let scheduler = Arc::new(RecordingScheduler::default());
    let mut runtime = runtime_with(&scheduler);

    eval(
        &mut runtime,
        "globalThis.count = 0; setTimeout(() => { globalThis.count += 1; }, 0);",
    );
    let token = scheduler.tokens()[0];

    assert_eq!(
        runtime.fire_timer(token).expect("first fire"),
        TimerFireOutcome::Fired { repeat: false }
    );
    assert_eq!(
        runtime.fire_timer(token).expect("second fire"),
        TimerFireOutcome::Missing,
        "a spent one-shot token must not re-run the callback"
    );
    assert_eq!(eval(&mut runtime, "globalThis.count"), "1");
}

#[test]
fn set_interval_stays_live_so_the_host_can_rearm_it() {
    let scheduler = Arc::new(RecordingScheduler::default());
    let mut runtime = runtime_with(&scheduler);

    eval(
        &mut runtime,
        "globalThis.ticks = 0; globalThis.handle = setInterval(() => { globalThis.ticks += 1; }, 10);",
    );

    let scheduled = scheduler.scheduled.lock().expect("scheduled").clone();
    assert_eq!(scheduled[0].2, Some(10), "setInterval reports its period");
    let token = scheduled[0].0;

    for expected in 1..=3 {
        assert_eq!(
            runtime.fire_timer(token).expect("interval fires"),
            TimerFireOutcome::Fired { repeat: true },
            "a repeating entry stays in the table for the host to re-arm"
        );
        assert_eq!(eval(&mut runtime, "globalThis.ticks"), expected.to_string());
    }

    eval(&mut runtime, "clearInterval(globalThis.handle)");
    assert_eq!(
        scheduler.cancelled.lock().expect("cancelled").as_slice(),
        &[token],
        "clearInterval reaches the host scheduler"
    );
    assert_eq!(
        runtime.fire_timer(token).expect("post-cancel fire"),
        TimerFireOutcome::Missing
    );
    assert_eq!(eval(&mut runtime, "globalThis.ticks"), "3");
}

#[test]
fn a_timer_callback_drains_its_own_microtasks_before_returning() {
    let scheduler = Arc::new(RecordingScheduler::default());
    let mut runtime = runtime_with(&scheduler);

    eval(
        &mut runtime,
        "globalThis.order = [];
         setTimeout(() => {
             globalThis.order.push('timer');
             queueMicrotask(() => globalThis.order.push('microtask'));
         }, 0);",
    );
    let token = scheduler.tokens()[0];

    runtime.fire_timer(token).expect("timer fires");

    assert_eq!(
        eval(&mut runtime, "globalThis.order.join(',')"),
        "timer,microtask",
        "fire_timer performs the microtask checkpoint for the task it ran"
    );
}

#[test]
fn a_throwing_timer_callback_surfaces_as_an_error_without_poisoning_the_runtime() {
    let scheduler = Arc::new(RecordingScheduler::default());
    let mut runtime = runtime_with(&scheduler);

    eval(
        &mut runtime,
        "setTimeout(() => { throw new TypeError('from timer'); }, 0);",
    );
    let token = scheduler.tokens()[0];

    let error = runtime
        .fire_timer(token)
        .expect_err("an unhandled throw is reported to the embedder");
    assert!(
        format!("{error:?}").contains("from timer"),
        "the diagnostic names the failing callback's throw: {error:?}"
    );

    assert_eq!(
        eval(&mut runtime, "1 + 1"),
        "2",
        "the isolate stays usable after a failed timer task"
    );
}

#[test]
fn without_a_scheduler_set_timeout_reports_the_missing_host_capability() {
    let mut runtime = Runtime::builder().build().expect("runtime builds");

    let error = runtime
        .eval(SourceInput::from_javascript("setTimeout(() => {}, 0)"))
        .expect_err("no scheduler installed");
    assert!(
        format!("{error:?}").contains("timer scheduler"),
        "the diagnostic names the missing host capability: {error:?}"
    );
}

#[test]
fn disposing_a_realm_cancels_every_timer_owned_by_that_realm() {
    let scheduler = Arc::new(RecordingScheduler::default());
    let mut runtime = runtime_with(&scheduler);
    let realm = runtime.create_realm().expect("realm");

    runtime
        .run_script_in_realm(
            realm,
            SourceInput::from_javascript(
                "setTimeout(() => { globalThis.mustNotRun = true; }, 1000);\
                 setInterval(() => { globalThis.mustNotRun = true; }, 1000);",
            ),
            "realm:timers",
        )
        .expect("schedule realm timers");
    let tokens = scheduler.tokens();
    assert_eq!(tokens.len(), 2);

    runtime.dispose_realm(realm).expect("dispose realm");
    assert_eq!(
        scheduler.cancelled.lock().expect("cancelled").as_slice(),
        tokens.as_slice(),
        "realm teardown must cancel host deadlines, not merely drop callbacks"
    );
    for token in tokens {
        assert_eq!(
            runtime.fire_timer(token).expect("late fire is harmless"),
            TimerFireOutcome::Missing
        );
    }
}
