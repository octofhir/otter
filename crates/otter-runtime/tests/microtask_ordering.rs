//! P2.2 Slice A + B regression coverage: deterministic microtask
//! and timer ordering across the runtime drain + run-until-idle
//! boundary.
//!
//! The fixture pins both the microtask half and the timer half, including the
//! microtask-precedes-timer ordering rule from HTML §8.1.5.5. Both halves drain
//! through the same runtime entry point (`IsolateRunner::process_message` →
//! `Runtime::fire_timer` / `Interpreter::drain_microtasks`).
//!
//! The microtask queue itself lives on `otter_vm::Interpreter`
//! because tasks carry parked frames + GC handles, which are
//! isolate-local by construction. Timers are scheduled host-side
//! by [`otter_runtime::handle::InboxTimerScheduler`] (the
//! Tokio-backed implementation of [`otter_vm::TimerScheduler`])
//! and resolved on the inbox hop back into the isolate runner.
//!
//! Spec:
//! - <https://tc39.es/ecma262/#sec-jobs-and-job-queues> (§9.4)
//! - <https://tc39.es/ecma262/#sec-promisereactionjob> (§27.2.1.3.2)
//! - <https://html.spec.whatwg.org/multipage/webappapis.html#microtask-queue>
//! - <https://html.spec.whatwg.org/multipage/timers-and-user-prompts.html#dom-settimeout>

use std::sync::{Arc, Mutex};

use otter_runtime::{ConsoleLevel, ConsoleSink, Otter, OtterError, SourceInput};

#[derive(Debug, Default)]
struct LogCapture {
    events: Mutex<Vec<String>>,
}

impl LogCapture {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn snapshot(&self) -> Vec<String> {
        self.events.lock().expect("log mutex").clone()
    }
}

impl ConsoleSink for LogCapture {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        if !matches!(level, ConsoleLevel::Log) {
            return;
        }
        let line = fields.join(" ");
        self.events.lock().expect("log mutex").push(line);
    }
}

fn run_script_capturing(source: &str) -> Vec<String> {
    let capture = LogCapture::new();
    let otter = Otter::builder()
        .console_sink(capture.clone())
        .build()
        .expect("otter build");
    otter
        .blocking_run_typescript(source)
        .expect("script must succeed");
    capture.snapshot()
}

async fn run_script_capturing_async(source: &str) -> Result<Vec<String>, OtterError> {
    let capture = LogCapture::new();
    let otter = Otter::builder()
        .console_sink(capture.clone())
        .build()
        .expect("otter build");
    otter
        .handle()
        .run_script(SourceInput::from_typescript(source), "<test>")
        .await?;
    Ok(capture.snapshot())
}

/// Plain `queueMicrotask` callbacks run after the synchronous
/// script finishes and observe FIFO order with respect to one
/// another (§9.4 + HTML microtask queue).
#[test]
fn queue_microtask_runs_after_script_in_fifo_order() {
    let log = run_script_capturing(
        r#"
            console.log("sync-1");
            queueMicrotask(() => console.log("micro-1"));
            queueMicrotask(() => console.log("micro-2"));
            queueMicrotask(() => console.log("micro-3"));
            console.log("sync-2");
        "#,
    );
    assert_eq!(
        log,
        vec![
            "sync-1".to_string(),
            "sync-2".to_string(),
            "micro-1".to_string(),
            "micro-2".to_string(),
            "micro-3".to_string(),
        ]
    );
}

/// A microtask that enqueues another microtask must observe its
/// own callback finish before the new one runs (FIFO append, not
/// LIFO push). Pins the swap-and-drain semantics in
/// `MicrotaskQueue::begin_drain` against accidental regression to
/// LIFO scheduling.
#[test]
fn microtask_enqueued_inside_microtask_runs_after_current_iteration() {
    let log = run_script_capturing(
        r#"
            queueMicrotask(() => {
                console.log("outer-start");
                queueMicrotask(() => console.log("nested"));
                console.log("outer-end");
            });
            queueMicrotask(() => console.log("sibling"));
        "#,
    );
    assert_eq!(
        log,
        vec![
            "outer-start".to_string(),
            "outer-end".to_string(),
            "sibling".to_string(),
            "nested".to_string(),
        ]
    );
}

/// Promise reaction handlers settle through the same drain path
/// as `queueMicrotask`. `then(a).then(b)` must observe `a` before
/// `b`, and a sibling `Promise.resolve().then(c)` chain interleaves
/// at the boundary defined by §27.2.1.3.2.
#[test]
fn promise_reaction_chain_drains_in_spec_fifo_order() {
    let log = run_script_capturing(
        r#"
            Promise.resolve()
                .then(() => console.log("a1"))
                .then(() => console.log("a2"))
                .then(() => console.log("a3"));
            Promise.resolve()
                .then(() => console.log("b1"))
                .then(() => console.log("b2"));
            console.log("sync");
        "#,
    );
    // Spec: each `then` schedules one microtask; the second `then`
    // in a chain only schedules after the first resolves. The
    // drain interleaves the two chains at every step.
    assert_eq!(
        log,
        vec![
            "sync".to_string(),
            "a1".to_string(),
            "b1".to_string(),
            "a2".to_string(),
            "b2".to_string(),
            "a3".to_string(),
        ]
    );
}

/// `queueMicrotask` interleaves with promise reaction microtasks
/// at insertion order — the queue has one logical FIFO, not two
/// per-source queues. Pins regression where a separate "promise
/// queue" might be introduced and accidentally reorder mixed
/// sources.
#[test]
fn queue_microtask_and_promise_then_share_a_single_fifo() {
    let log = run_script_capturing(
        r#"
            Promise.resolve().then(() => console.log("p1"));
            queueMicrotask(() => console.log("q1"));
            Promise.resolve().then(() => console.log("p2"));
            queueMicrotask(() => console.log("q2"));
        "#,
    );
    assert_eq!(
        log,
        vec![
            "p1".to_string(),
            "q1".to_string(),
            "p2".to_string(),
            "q2".to_string(),
        ]
    );
}

// ---- Slice B: timer ordering ----

/// `setTimeout(fn, 0)` runs after every queued microtask drains —
/// HTML §8.1.5.5 mandates that microtasks always finish before the
/// next "task" (including a timer task) starts. Pinning this with
/// a `Promise.resolve().then` reaction enqueued *after* the
/// timer schedules: the `then` handler must still observe before
/// the timer callback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn microtasks_drain_before_zero_delay_timer_task() {
    let log = run_script_capturing_async(
        r#"
            console.log("sync-1");
            setTimeout(() => console.log("timer-0"), 0);
            Promise.resolve().then(() => console.log("micro-1"));
            queueMicrotask(() => console.log("micro-2"));
            console.log("sync-2");
        "#,
    )
    .await
    .expect("script must succeed");
    assert_eq!(
        log,
        vec![
            "sync-1".to_string(),
            "sync-2".to_string(),
            "micro-1".to_string(),
            "micro-2".to_string(),
            "timer-0".to_string(),
        ]
    );
}

/// Multiple `setTimeout(fn, 0)` calls run in the order they were
/// scheduled (host-side: the inbox channel preserves FIFO). A
/// microtask enqueued *inside* the first timer callback runs
/// before the second timer fires — HTML §8.1.5.5 again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timer_scheduling_is_fifo_and_microtasks_drain_between_tasks() {
    let log = run_script_capturing_async(
        r#"
            setTimeout(() => {
                console.log("timer-a");
                queueMicrotask(() => console.log("inner-micro"));
            }, 0);
            setTimeout(() => console.log("timer-b"), 0);
        "#,
    )
    .await
    .expect("script must succeed");
    assert_eq!(
        log,
        vec![
            "timer-a".to_string(),
            "inner-micro".to_string(),
            "timer-b".to_string(),
        ]
    );
}

/// `clearTimeout(token)` cancels a pending one-shot timer. The
/// host scheduler de-arms the Tokio sleep AND the per-isolate
/// `TimerCallbacks` table forgets the entry, so even a late fire
/// (lost the cancel race) becomes a no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleartimeout_suppresses_pending_callback() {
    let log = run_script_capturing_async(
        r#"
            const t = setTimeout(() => console.log("should-not-fire"), 0);
            clearTimeout(t);
            queueMicrotask(() => console.log("micro-only"));
        "#,
    )
    .await
    .expect("script must succeed");
    assert_eq!(log, vec!["micro-only".to_string()]);
}

/// `setInterval` is a repeating task source, not a one-shot
/// timeout with a retained callback entry. The ref'd timer keeps
/// `run_until_idle` alive until `clearInterval`, and each tick
/// gets its own microtask checkpoint before the next timer task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setinterval_repeats_until_clearinterval_and_checkpoints_each_tick() {
    let log = run_script_capturing_async(
        r#"
            let count = 0;
            const id = setInterval(() => {
                count += 1;
                console.log("tick-" + count);
                queueMicrotask(() => console.log("micro-" + count));
                if (count === 3) clearInterval(id);
            }, 1);
        "#,
    )
    .await
    .expect("script must succeed");
    assert_eq!(
        log,
        vec![
            "tick-1".to_string(),
            "micro-1".to_string(),
            "tick-2".to_string(),
            "micro-2".to_string(),
            "tick-3".to_string(),
            "micro-3".to_string(),
        ]
    );
}

/// Direct/blocking embedders that never installed a host-side
/// timer scheduler observe a `TypeError` from `setTimeout` —
/// silent drops would let scripts deadlock waiting for callbacks
/// the runtime can never fire. Pinned through the sync
/// [`otter_runtime::Runtime`] facade to lock down the negative
/// path; the inbox-runner positive path is exercised by the
/// other timer tests above.
#[test]
fn settimeout_without_scheduler_throws_typeerror() {
    use otter_runtime::Runtime;
    let mut rt = Runtime::builder().build().expect("runtime");
    let err = rt
        .run_script(
            SourceInput::from_javascript("setTimeout(() => {}, 0);"),
            "<no-scheduler>",
        )
        .expect_err("must reject without a scheduler");
    let msg = match err {
        OtterError::Runtime { diagnostic } => diagnostic.message,
        OtterError::Compile { diagnostics } => diagnostics
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("expected Runtime/Compile error, got {other:?}"),
    };
    assert!(
        msg.contains("timer scheduler"),
        "missing scheduler diagnostic, got {msg:?}"
    );
}

/// A microtask that throws settles its result capability (when
/// scheduled as a promise reaction) by routing the abrupt
/// completion's [[Value]] into the downstream rejection per
/// §27.2.1.3.2 PromiseReactionJob, and does not block sibling
/// chains on the same drain.
#[test]
fn promise_then_handler_throw_routes_value_into_catch_and_unblocks_siblings() {
    let log = run_script_capturing(
        r#"
            Promise.resolve()
                .then(() => { throw "a-fail"; })
                .catch((reason) => console.log("caught-string:" + reason));
            Promise.resolve()
                .then(() => { throw new Error("a-error"); })
                .catch((reason) => console.log("caught-error:" + reason.message));
            Promise.resolve()
                .then(() => console.log("sibling-ok"));
        "#,
    );
    assert!(
        log.contains(&"caught-string:a-fail".to_string()),
        "string throw must round-trip its value through catch, got {log:?}"
    );
    assert!(
        log.contains(&"caught-error:a-error".to_string()),
        "Error throw must preserve its `.message` through catch, got {log:?}"
    );
    assert!(
        log.contains(&"sibling-ok".to_string()),
        "sibling chain blocked, got {log:?}"
    );
}
