//! Isolate scheduling, timers and host globals.
//!
//! # Contents
//! Regression tests through the runtime embedding API.
//!
//! # Invariants
//! Each test constructs its own runtime.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use super::*;
use crate::event_loop::{TimerRequest, TimerToken};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn otter_clone_runs_from_tokio_worker_tasks() {
    let otter = Otter::new();
    let mut joins = Vec::new();
    for i in 0..8 {
        let otter = otter.clone();
        joins.push(tokio::spawn(async move {
            let source = format!("{i};");
            let result = otter.run_script(&source).await.unwrap();
            result.completion_string().to_string()
        }));
    }

    let mut completions = Vec::new();
    for join in joins {
        completions.push(join.await.unwrap());
    }
    completions.sort();
    assert_eq!(completions, ["0", "1", "2", "3", "4", "5", "6", "7"]);

    let stats = otter.activity_stats();
    assert_eq!(stats.submitted_commands, 8);
    assert_eq!(stats.completed_commands, 8);
    assert_eq!(stats.failed_commands, 0);
    assert_eq!(stats.queued_commands, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn otter_async_run_module_uses_handle_runner() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("dep.ts"), "export const value = 41;\n").unwrap();
    std::fs::write(
            dir.path().join("entry.ts"),
            "import { value } from \"./dep.ts\";\nfunction fail() { return undefined.x; }\nif (value !== 41) fail();\n",
        )
        .unwrap();

    let otter = Otter::new();
    let result = otter.run_module(dir.path().join("entry.ts")).await.unwrap();

    assert_eq!(result.completion_string(), "undefined");
}

#[test]
fn runtime_installs_console_global() {
    let otter = Otter::new();
    let result = otter
        .blocking_run_typescript(
            r#"
                function fail(msg) { throw new Error(msg); }
                if (typeof console !== "object") fail("missing console");
                if (typeof console.log !== "function") fail("missing console.log");
                if (typeof console.error !== "function") fail("missing console.error");
                console.log("runtime-console", 7);
                console.warn(new Error("warn-path"));
                console.assert(true, "should not print");
                console.assert(false, "assert-path");
                "#,
        )
        .unwrap();

    assert_eq!(result.completion_string(), "undefined");
}

type ConsoleEvents = std::sync::Arc<std::sync::Mutex<Vec<(otter_vm::ConsoleLevel, Vec<String>)>>>;

#[derive(Debug)]
struct CapturingConsole {
    events: ConsoleEvents,
}

impl otter_vm::ConsoleSink for CapturingConsole {
    fn write(&self, level: otter_vm::ConsoleLevel, fields: &[String]) {
        self.events.lock().unwrap().push((level, fields.to_vec()));
    }
}

#[test]
fn runtime_console_sink_is_embedder_overridable() {
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::new(CapturingConsole {
        events: events.clone(),
    });
    let otter = Otter::builder().console_sink(sink).build().unwrap();
    let result = otter
        .blocking_run_typescript(
            r#"
                console.log("hello", 7);
                console.warn("careful");
                console.assert(false, "nope");
                "#,
        )
        .unwrap();

    assert_eq!(result.completion_string(), "undefined");
    let captured = events.lock().unwrap();
    assert_eq!(captured.len(), 3);
    assert_eq!(captured[0].0, otter_vm::ConsoleLevel::Log);
    assert_eq!(captured[0].1, vec!["hello".to_string(), "7".to_string()]);
    assert_eq!(captured[1].0, otter_vm::ConsoleLevel::Warn);
    assert_eq!(captured[1].1, vec!["careful".to_string()]);
    assert_eq!(captured[2].0, otter_vm::ConsoleLevel::Assert);
    assert_eq!(
        captured[2].1,
        vec!["Assertion failed".to_string(), "nope".to_string()]
    );
}

#[test]
fn runtime_installs_math_namespace_from_static_surface_spec() {
    let otter = Otter::new();
    let result = otter
        .blocking_run_typescript(
            r#"
                function fail(msg) { throw new Error(msg); }
                const pi = Object.getOwnPropertyDescriptor(Math, "PI");
                if (pi.writable !== false) fail("PI writable");
                if (pi.enumerable !== false) fail("PI enumerable");
                if (pi.configurable !== false) fail("PI configurable");
                const abs = Math.abs;
                if (typeof abs !== "function") fail("missing Math.abs");
                if (abs.length !== 1) fail("bad Math.abs length");
                if (abs(-7) !== 7) fail("bad extracted Math.abs");
                "#,
        )
        .unwrap();

    assert_eq!(result.completion_string(), "undefined");
}

#[test]
fn runtime_installs_json_and_atomics_namespaces_from_static_surface_specs() {
    let otter = Otter::new();
    let result = otter
        .blocking_run_typescript(
            r#"
                function fail(msg) { throw new Error(msg); }

                const parse = JSON.parse;
                if (typeof parse !== "function") fail("missing JSON.parse");
                if (parse.length !== 2) fail("bad JSON.parse length");
                if (parse("{\"x\":3}").x !== 3) fail("bad extracted JSON.parse");

                const stringify = JSON.stringify;
                if (stringify.length !== 3) fail("bad JSON.stringify length");
                if (stringify({ x: 4 }) !== "{\"x\":4}") fail("bad extracted JSON.stringify");

                const isLockFree = Atomics.isLockFree;
                if (typeof isLockFree !== "function") fail("missing Atomics.isLockFree");
                if (isLockFree.length !== 1) fail("bad Atomics.isLockFree length");
                if (isLockFree(4) !== true) fail("bad extracted Atomics.isLockFree");
                "#,
        )
        .unwrap();

    assert_eq!(result.completion_string(), "undefined");
}

#[test]
fn top_level_await_rejection_renders_error_to_string() {
    let otter = Otter::new();
    let err = otter
        .blocking_run_typescript(
            r#"
                await Promise.resolve();
                throw new Error("boom");
                "#,
        )
        .unwrap_err();

    match err {
        OtterError::Runtime { diagnostic } => {
            assert_eq!(diagnostic.code, "UNCAUGHT");
            // The rendering is the error's stack: header line first,
            // frame lines after, exactly what Node prints.
            assert!(
                diagnostic
                    .message
                    .starts_with("uncaught exception: Error: boom"),
                "unexpected message: {}",
                diagnostic.message
            );
            assert!(
                diagnostic.message.contains("\n    at "),
                "expected frame lines, got: {}",
                diagnostic.message
            );
        }
        other => panic!("expected Runtime, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_handle_timers_and_diagnostics_update_stats() {
    let otter = Otter::new();
    let handle = otter.handle().clone();

    let token = handle.schedule_timer(TimerRequest {
        delay: Duration::from_millis(10),
        repeat: None,
    });
    assert!(!handle.cancel_timer(TimerToken(token.0 + 1)));
    handle.complete_dynamic_module_job_for_tests();
    handle.wake_runtime("runtime-test");

    tokio::time::sleep(Duration::from_millis(50)).await;
    let stats = handle.activity_stats();

    assert_eq!(stats.pending_ref_timers, 0);
    assert_eq!(stats.fired_timers, 1);
    assert_eq!(stats.pending_dynamic_module_jobs, 0);
    assert_eq!(stats.completed_dynamic_module_jobs, 1);
    assert_eq!(stats.diagnostics, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_keep_alive_liveness_is_idempotent() {
    let otter = Otter::new();
    let handle = otter.handle().clone();

    let resource = handle.retain_keep_alive(RuntimeLiveness::Ref);
    assert_eq!(handle.activity_stats().pending_ref_host_ops, 1);
    assert!(!resource.is_closed());

    resource.unref();
    let stats = handle.activity_stats();
    assert_eq!(stats.pending_ref_host_ops, 0);
    assert_eq!(stats.pending_unref_host_ops, 1);
    assert_eq!(stats.completed_host_ops, 0);
    assert_eq!(stats.cancelled_host_ops, 0);

    resource.ref_();
    resource.ref_();
    let stats = handle.activity_stats();
    assert_eq!(stats.pending_ref_host_ops, 1);
    assert_eq!(stats.pending_unref_host_ops, 0);
    assert_eq!(stats.completed_host_ops, 0);
    assert_eq!(stats.cancelled_host_ops, 0);

    resource.close();
    resource.close();
    assert!(resource.is_closed());
    let stats = handle.activity_stats();
    assert_eq!(stats.pending_ref_host_ops, 0);
    assert_eq!(stats.pending_unref_host_ops, 0);
    assert_eq!(stats.completed_host_ops, 1);
    assert_eq!(stats.cancelled_host_ops, 0);

    let unref = handle.retain_keep_alive(RuntimeLiveness::Unref);
    assert_eq!(handle.activity_stats().pending_unref_host_ops, 1);
    drop(unref);
    let stats = handle.activity_stats();
    assert_eq!(stats.pending_unref_host_ops, 0);
    assert_eq!(stats.cancelled_host_ops, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_task_runs_on_isolate_loop() {
    struct NotifyTask(std::sync::mpsc::Sender<()>);

    impl RuntimeTask for NotifyTask {
        fn run(self: Box<Self>, _runtime: &mut Runtime) -> Result<(), OtterError> {
            self.0
                .send(())
                .expect("runtime task receiver should be alive");
            Ok(())
        }
    }

    let otter = Otter::new();
    let handle = otter.handle().clone();
    let (tx, rx) = std::sync::mpsc::channel();

    handle
        .enqueue_runtime_task(NotifyTask(tx), RuntimeLiveness::Ref)
        .expect("runtime task should enqueue");

    rx.recv_timeout(Duration::from_secs(5))
        .expect("runtime task should run");
    let mut stats = handle.activity_stats();
    for _ in 0..50 {
        if stats.pending_ref_host_ops == 0 && stats.completed_host_ops == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        stats = handle.activity_stats();
    }
    assert_eq!(stats.pending_ref_host_ops, 0);
    assert_eq!(stats.completed_host_ops, 1);
}

#[test]
fn async_function_adopts_returned_native_promise() {
    let mut runtime = Runtime::builder().build().unwrap();
    runtime
        .eval(SourceInput::from_javascript(
            r#"
                let out = "pending";
                async function inner() {
                  return Promise.resolve("adopted");
                }
                inner().then((value) => {
                  out = value;
                });
                "#,
        ))
        .unwrap();
    let result = runtime.eval(SourceInput::from_javascript("out")).unwrap();
    assert_eq!(result.completion_string(), "adopted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_handle_timer_cancellation_and_timeout_are_observable() {
    let otter = Otter::builder()
        .timeout(Duration::from_millis(20))
        .build()
        .unwrap();
    let handle = otter.handle().clone();
    let token = handle.schedule_timer(TimerRequest {
        delay: Duration::from_secs(60),
        repeat: None,
    });
    assert!(handle.cancel_timer(token));

    let err = otter.run_script("while (true) {}").await.unwrap_err();
    assert!(matches!(err, OtterError::Timeout { .. }));

    let result = otter.run_script("1 + 1;").await.unwrap();
    assert_eq!(result.completion_string(), "2");

    let stats = otter.activity_stats();
    assert_eq!(stats.cancelled_timers, 1);
    assert_eq!(stats.timed_out_commands, 1);
    assert!(stats.failed_commands >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_handle_bounded_queue_reports_backpressure() {
    let config = RuntimeConfig {
        timeout: Duration::from_millis(100),
        ..RuntimeConfig::default()
    };
    let handle = RuntimeHandle::spawn_with_capacity(config, 1).unwrap();

    let first = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .run_script(SourceInput::from_javascript("while (true) {}"), "<busy>")
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;

    let second = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .run_script(SourceInput::from_javascript("1;"), "<queued>")
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;

    let err = handle
        .run_script(SourceInput::from_javascript("2;"), "<overflow>")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        OtterError::Internal { code, .. } if code == "RUNTIME_BACKPRESSURE"
    ));

    assert!(matches!(
        first.await.unwrap(),
        Err(OtterError::Timeout { .. })
    ));
    assert_eq!(second.await.unwrap().unwrap().completion_string(), "1");
    assert_eq!(handle.activity_stats().backpressure_rejections, 1);
}
