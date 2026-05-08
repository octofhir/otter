//! Per-test isolation primitives: fresh `Runtime` factory, watchdog
//! thread + cooperative cancellation, and `catch_unwind` crash trap.
//!
//! The runner allocates a fresh `Runtime` for every test so a
//! poisoned global cannot leak across tests.
//! This module isolates that machinery so the per-test driver in
//! [`crate::runner`] stays mechanical.
//!
//! # Hardening layers
//!
//! 1. **Cooperative cancellation.** A watchdog thread holds an
//!    [`otter_runtime::InterruptHandle`] clone and trips it when
//!    the per-test wall-clock budget expires. The interpreter
//!    polls the flag at every back-edge and surfaces
//!    [`otter_runtime::OtterError::Interrupted`].
//! 2. **Heap cap.** Each `Runtime` is built with `max_heap_bytes`;
//!    the engine surfaces [`otter_runtime::OtterError::OutOfMemory`]
//!    when the cap fires.
//! 3. **Crash trap.** [`std::panic::catch_unwind`] wraps every
//!    engine call in [`run_with_watchdog`] so a single panicking
//!    test cannot derail the suite.
//!
//! Spec link: <https://tc39.es/ecma262/>

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use otter_runtime::{ExecutionResult, InterruptHandle, OtterError, Runtime, SourceInput};

/// Build a fresh runtime with the configured per-test caps.
///
/// `max_heap_bytes = 0` disables the cap (matches
/// `RuntimeBuilder::max_heap_bytes`'s contract).
pub fn fresh_runtime(timeout: Duration, max_heap_bytes: u64) -> Result<Runtime, OtterError> {
    Runtime::builder()
        .timeout(timeout)
        .max_heap_bytes(max_heap_bytes)
        .build()
}

/// Outcome of [`run_with_watchdog`] when the engine call returned.
///
/// Distinct from [`crate::runner::Outcome`] so the per-test driver
/// can layer extra metadata (negative-test inversion, peak heap,
/// etc.) on top.
#[derive(Debug)]
pub enum WatchdogOutcome {
    /// Engine returned normally.
    Ok(ExecutionResult),
    /// Engine returned an error (timeout / OOM / runtime / compile).
    Err(OtterError),
    /// Watchdog fired before the engine returned. The
    /// [`OtterError::Interrupted`] return path is the canonical
    /// signal but we surface the `wall_ms` separately so the report
    /// can record how long the runaway test was allowed to run.
    Timeout {
        /// Wall-clock milliseconds the runaway test was allowed
        /// to run before the watchdog fired.
        wall_ms: u64,
    },
    /// Engine panicked. The string carries the formatted payload
    /// (matches `panic_payload_to_string`).
    Panic(String),
}

/// Run `body` with cooperative cancellation enforced by a watchdog
/// thread. The watchdog sleeps until `timeout` elapses, then trips
/// `runtime.interrupt_handle().interrupt()`. The body's call into
/// the engine returns [`OtterError::Interrupted`] which the wrapper
/// reclassifies as [`WatchdogOutcome::Timeout`].
///
/// `timeout = Duration::ZERO` disables the watchdog entirely (used
/// in tests that want to deliberately stress without a deadline).
pub fn run_with_watchdog<F>(runtime: &mut Runtime, timeout: Duration, body: F) -> WatchdogOutcome
where
    F: FnOnce(&mut Runtime) -> Result<ExecutionResult, OtterError>,
{
    let interrupt = runtime.interrupt_handle();
    let watchdog = if timeout > Duration::ZERO {
        Some(spawn_watchdog(interrupt.clone(), timeout))
    } else {
        None
    };
    let start = Instant::now();
    // §10.4.3.1 InterruptCheck rides through `Interpreter::run`,
    // so the engine call observes the flag without us having to
    // poll it ourselves.
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| body(runtime)));
    let wall_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Some(handle) = watchdog {
        handle.cancel();
    }
    match result {
        Ok(Ok(exec)) => WatchdogOutcome::Ok(exec),
        Ok(Err(OtterError::Interrupted)) => WatchdogOutcome::Timeout { wall_ms },
        Ok(Err(other)) => WatchdogOutcome::Err(other),
        Err(payload) => WatchdogOutcome::Panic(panic_payload_to_string(payload)),
    }
}

/// Convenience: build a [`SourceInput`] from a JavaScript string.
#[must_use]
pub fn js_source(text: impl Into<String>) -> SourceInput {
    SourceInput::from_javascript(text)
}

struct WatchdogHandle {
    cancelled: Arc<AtomicBool>,
    sender: Option<mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl WatchdogHandle {
    fn cancel(mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(tx) = self.sender.take() {
            // Best-effort wakeup. Receiver may already have woken
            // and exited — drop the result.
            let _ = tx.send(());
        }
        if let Some(handle) = self.join.take() {
            // Watchdog is short-lived; if the test has already
            // returned the watchdog races to its `recv_timeout`
            // boundary and exits. `join` is bounded.
            let _ = handle.join();
        }
    }
}

fn spawn_watchdog(handle: InterruptHandle, timeout: Duration) -> WatchdogHandle {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<()>();
    let cancelled_in_thread = Arc::clone(&cancelled);
    let join = thread::Builder::new()
        .name("test262-watchdog".to_string())
        .spawn(move || {
            // Wait either for the cancellation signal or the
            // wall-clock deadline. `recv_timeout` returns `Err` on
            // timeout — that is the trip path.
            let outcome = rx.recv_timeout(timeout);
            if cancelled_in_thread.load(Ordering::SeqCst) {
                return;
            }
            if outcome.is_err() {
                handle.interrupt();
            }
        })
        .expect("test262-watchdog thread should spawn");
    WatchdogHandle {
        cancelled,
        sender: Some(tx),
        join: Some(join),
    }
}

fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "engine panic (non-string payload)".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_returns_ok_when_body_finishes_quickly() {
        let mut rt = fresh_runtime(Duration::from_secs(5), 64 * 1024 * 1024).unwrap();
        let outcome = run_with_watchdog(&mut rt, Duration::from_secs(5), |rt| {
            rt.run_script(SourceInput::from_javascript("1 + 1;"), "<test>")
        });
        assert!(matches!(outcome, WatchdogOutcome::Ok(_)));
    }

    #[test]
    fn watchdog_classifies_panic_as_panic() {
        let mut rt = fresh_runtime(Duration::ZERO, 64 * 1024 * 1024).unwrap();
        let outcome = run_with_watchdog(&mut rt, Duration::ZERO, |_| {
            panic!("synthetic engine panic for test")
        });
        match outcome {
            WatchdogOutcome::Panic(msg) => assert!(msg.contains("synthetic")),
            other => panic!("expected Panic, got {other:?}"),
        }
    }

    #[test]
    fn watchdog_with_zero_timeout_does_not_spawn_thread() {
        // Zero timeout disables the watchdog; this regresses on the
        // "watchdog always spawns" implementation if we ever flip it.
        let mut rt = fresh_runtime(Duration::ZERO, 64 * 1024 * 1024).unwrap();
        let outcome = run_with_watchdog(&mut rt, Duration::ZERO, |rt| {
            rt.run_script(SourceInput::from_javascript("var x = 1;"), "<test>")
        });
        assert!(matches!(outcome, WatchdogOutcome::Ok(_)));
    }
}
