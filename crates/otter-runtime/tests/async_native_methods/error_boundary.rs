//! Async host errors must reach the existing runtime task and turn boundary.
//!
//! # Contents
//! - Structural/control failures produced by a genuinely suspended native future.
//! - Isolate-thread IntoJs and post-settlement microtask drain failure.
//! - Intrinsic rejection classes after mutable global replacement.
//! - Actual oversized diagnostic OOM and process-exit completion.
//!
//! # Invariants
//! - Each suspended job traverses the production sink, inbox and RuntimeTask.
//! - Failed jobs have exactly one failed terminal credit and no pending host hold.
//! - Tests observe the original Result; no panic in an engine ABI callback is used.
//!
//! # See also
//! - `otter_vm::marshal::PromiseCompleter` owns conversion and settlement.
//! - `otter_runtime::Runtime::run_host_completion` returns the actual turn failure.

use super::*;
use otter_runtime::{DiagnosticCode, DiagnosticKind};

fn assert_structural(error: &OtterError) {
    assert!(
        matches!(error, OtterError::Internal { code, .. }
        if code == DiagnosticCode::VmBytecodeInvariant.as_str()),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspended_structural_and_control_results_fail_the_actual_runtime_job_once() {
    for (expression, expected) in [
        ("s.fatal(0)", 0),
        ("s.fatal(1)", 0),
        ("s.fatal(2)", 1),
        ("s.fatal(3)", 2),
        ("s.conversionFailure()", 0),
    ] {
        let capture = LogCapture::new();
        let otter = build_otter(capture.clone());
        let source = format!(
            "const s = new Sleeper('fatal'); {expression}.then(() => console.log('fulfilled'), () => console.log('rejected')); "
        );
        let error = otter
            .handle()
            .run_script(
                SourceInput::from_javascript(&source),
                "<async-fatal-boundary>",
            )
            .await
            .expect_err("structural/control result must fail its actual runtime job");
        match expected {
            0 => assert_structural(&error),
            1 => assert!(matches!(error, OtterError::Interrupted), "{error:?}"),
            _ => assert!(
                matches!(error, OtterError::Runtime { ref diagnostic }
                if diagnostic.kind == DiagnosticKind::Timeout
                && diagnostic.code == DiagnosticCode::BudgetExceeded.as_str()
                && diagnostic.message == "async budget detail"),
                "{error:?}"
            ),
        }
        assert!(
            capture.snapshot().is_empty(),
            "fatal errors never become promise rejections"
        );
        let stats = otter.activity_stats();
        assert_eq!(stats.pending_ref_host_ops, 0);
        assert_eq!(stats.pending_unref_host_ops, 0);
        assert_eq!(
            stats.failed_host_ops, 1,
            "one actual failed completion credit"
        );
        assert_eq!(stats.completed_host_ops, 0);
    }
}

#[test]
fn layer_a_returns_the_actual_post_settlement_microtask_drain_failure() {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Tokio runtime");
    let capture = LogCapture::new();
    let (mut runtime, completions) = build_layer_a(capture.clone(), executor.handle().clone());
    runtime
        .run_script(
            SourceInput::from_javascript(
                "new Sleeper('drain').wait(1).then(() => new Sleeper('reaction').failSync());",
            ),
            "<host-drain-failure>",
        )
        .expect("script starts a suspended host operation");
    let (admission, job, outcome) = completions
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("actual posted completion");
    assert_eq!(
        outcome,
        HostCompletionOutcome::Completed,
        "host future itself produced a normal result"
    );
    let error = runtime
        .run_host_completion(job)
        .expect_err("reaction drain's structural error must leave the job");
    drop(admission);
    assert_structural(&error);
    assert!(capture.snapshot().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspended_rejection_uses_original_intrinsic_class_without_calling_changed_globals()
-> Result<(), OtterError> {
    let capture = LogCapture::new();
    let otter = build_otter(capture.clone());
    otter.handle().run_script(SourceInput::from_javascript(r#"
        const original = SyntaxError.prototype;
        let calls = 0;
        globalThis.Error = globalThis.TypeError = globalThis.SyntaxError = function changed() { calls++; throw 997; };
        new Sleeper('syntax').syntax(false).catch(error => {
            console.log(error.name + ':' + error.message);
            console.log(Object.getPrototypeOf(error) === original);
            console.log(calls);
        });
    "#), "<async-intrinsic-rejection>").await?;
    assert_eq!(
        capture.snapshot(),
        vec![
            "SyntaxError:Sleeper.syntax: original syntax payload",
            "true",
            "0"
        ]
    );
    let stats = otter.activity_stats();
    assert_eq!(stats.pending_ref_host_ops, 0);
    assert_eq!(stats.completed_host_ops, 1);
    assert_eq!(stats.failed_host_ops, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspended_rejection_materialization_oom_is_the_actual_failed_runtime_result() {
    let cap = 4 * 1024 * 1024;
    let capture = LogCapture::new();
    let otter = Otter::builder()
        .max_heap_bytes(cap)
        .console_sink(capture.clone())
        .global_classes([GlobalClass::from_intrinsic::<SleeperIntrinsic>()])
        .build()
        .expect("capped async runtime");
    let error = otter.handle().run_script(SourceInput::from_javascript(
        "new Sleeper('large').syntax(true).then(() => console.log('fulfilled'), () => console.log('fabricated rejection'));"
    ), "<async-materialization-oom>").await.expect_err("diagnostic allocation OOM must leave the actual runtime task");
    assert!(
        matches!(error, OtterError::OutOfMemory { requested_bytes, heap_limit_bytes }
        if requested_bytes > cap && heap_limit_bytes == cap),
        "{error:?}"
    );
    assert!(capture.snapshot().is_empty());
    let stats = otter.activity_stats();
    assert_eq!(stats.pending_ref_host_ops, 0);
    assert_eq!(stats.failed_host_ops, 1);
    assert_eq!(stats.completed_host_ops, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspended_process_exit_keeps_the_original_exit_code() -> Result<(), OtterError> {
    let capture = LogCapture::new();
    let otter = build_otter(capture.clone());
    let result = otter
        .handle()
        .run_script(
            SourceInput::from_javascript(
                "new Sleeper('exit').fatal(4).catch(() => console.log('rejected exit'));",
            ),
            "<async-exit-boundary>",
        )
        .await?;
    assert_eq!(result.exit_code(), 27);
    assert!(
        capture.snapshot().is_empty(),
        "process exit is never a rejection"
    );
    assert_eq!(otter.activity_stats().pending_ref_host_ops, 0);
    Ok(())
}

#[test]
fn atomics_notify_job_returns_the_actual_source_context_reaction_failure() {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Tokio runtime");
    let capture = LogCapture::new();
    let (mut runtime, completions) = build_layer_a(capture.clone(), executor.handle().clone());
    runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
        const buffer = new SharedArrayBuffer(4);
        const words = new Int32Array(buffer);
        const waiter = Atomics.waitAsync(words, 0, 0);
        if (!waiter.async) throw 719;
        waiter.value.then(() => new Sleeper('Atomics reaction').failSync());
        console.log(Atomics.notify(words, 0, 1));
    "#,
            ),
            "<atomics-host-drain-failure>",
        )
        .expect("real parked waiter and notify");
    assert_eq!(capture.snapshot(), vec!["1"]);
    let (admission, job, outcome) = completions
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("actual notify completion");
    assert_eq!(outcome, HostCompletionOutcome::Completed);
    let error = runtime
        .run_host_completion(job)
        .expect_err("notify reaction's original failure");
    drop(admission);
    assert_structural(&error);
    assert_eq!(capture.snapshot(), vec!["1"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handled_rejection_cannot_supply_a_later_fatal_reaction_diagnostic() {
    let capture = LogCapture::new();
    let otter = build_otter(capture.clone());
    let error = otter.handle().run_script(SourceInput::from_javascript(
        "new Sleeper('syntax').syntax(false).catch(() => new Sleeper('reaction').failSync());"
    ), "<handled-rejection-fatal-reaction>").await
        .expect_err("actual fatal reaction leaves its suspended host job");
    assert_structural(&error);
    let OtterError::Internal { message, .. } = error else {
        unreachable!()
    };
    assert_eq!(
        message,
        otter_vm::VmError::InvalidOperand.to_string(),
        "the handled syntax rejection's detail cannot describe the next structural failure"
    );
    assert!(!message.contains("syntax payload"));
    assert!(capture.snapshot().is_empty());
    let stats = otter.activity_stats();
    assert_eq!(stats.failed_host_ops, 1);
    assert_eq!(stats.completed_host_ops, 0);
    assert_eq!(stats.pending_ref_host_ops, 0);
}

#[test]
fn async_module_ancestor_fatal_and_actual_error_allocation_oom_leave_the_turn() {
    for (call, cap) in [("failSync", None), ("largeSync", Some(4 * 1024 * 1024))] {
        let directory = tempfile::tempdir().expect("async module fixture directory");
        let dependency = directory.path().join("dependency.mjs");
        let entry = directory.path().join("entry.mjs");
        std::fs::write(
            &dependency,
            "await Promise.resolve(719); export const marker = 719;",
        )
        .expect("write real top-level-await dependency");
        std::fs::write(&entry, format!(
            "import {{ marker }} from './dependency.mjs'; if (marker !== 719) throw 997; new Sleeper('module ancestor').{call}(); console.log('must not complete');"
        )).expect("write parent whose body runs after async dependency completion");
        let capture = LogCapture::new();
        let mut builder = Otter::builder()
            .console_sink(capture.clone())
            .global_classes([GlobalClass::from_intrinsic::<SleeperIntrinsic>()]);
        if let Some(cap) = cap {
            builder = builder.max_heap_bytes(cap);
        }
        let otter = builder.build().expect("async module runtime");
        let error = otter
            .blocking_run_file(&entry)
            .expect_err("the actual module-fulfilled reaction cannot fabricate a rejection reason");
        if let Some(cap) = cap {
            assert!(
                matches!(error, OtterError::OutOfMemory { requested_bytes, heap_limit_bytes }
                if requested_bytes > cap && heap_limit_bytes == cap),
                "{error:?}"
            );
        } else {
            assert_structural(&error);
        }
        assert!(
            capture.snapshot().is_empty(),
            "failed parent has no completed body"
        );
        assert_eq!(otter.activity_stats().pending_ref_host_ops, 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_import_specifier_fatal_is_not_a_catchable_type_error() {
    let capture = LogCapture::new();
    let otter = build_otter(capture.clone());
    let error = otter
        .handle()
        .run_script(
            SourceInput::from_javascript(
                r#"
        import({ toString() { new Sleeper('specifier').failSync(); return './unreached.mjs'; } })
            .catch(() => console.log('fabricated import rejection'));
    "#,
            ),
            "<dynamic-import-fatal-specifier>",
        )
        .await
        .expect_err("specifier structural failure is the actual source turn result");
    assert_structural(&error);
    assert!(capture.snapshot().is_empty());
    assert_eq!(otter.activity_stats().pending_ref_host_ops, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collection_iterator_return_fatal_cannot_be_suppressed_by_original_throw() {
    let capture = LogCapture::new();
    let otter = build_otter(capture.clone());
    let error = otter
        .handle()
        .run_script(
            SourceInput::from_javascript(
                r#"
        const original = { marker: 719 };
        Set.prototype.add = function () { throw original; };
        const iterator = {
            [Symbol.iterator]() { return this; },
            next() { return { done: false, value: 31 }; },
            return() { new Sleeper('Iterator.return').failSync(); return {}; }
        };
        try { new Set(iterator); } catch (error) { console.log('suppressed fatal'); }
    "#,
            ),
            "<collection-iterator-close-fatal>",
        )
        .await
        .expect_err("actual collection constructor must return Iterator.return structural failure");
    assert_structural(&error);
    assert!(capture.snapshot().is_empty());
    assert_eq!(otter.activity_stats().pending_ref_host_ops, 0);
}

#[allow(dead_code)]
#[path = "../support/moving_children.rs"]
mod close_motion;

#[test]
fn from_async_return_get_and_call_gc_do_not_leak_cleanup_diagnostics_into_fatal_reaction() {
    use otter_runtime::{
        RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeValue,
    };
    use std::sync::atomic::AtomicUsize;
    for getter in [false, true] {
        let capture = LogCapture::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let observations = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = Runtime::builder()
            .console_sink(capture.clone())
            .global_classes([GlobalClass::from_intrinsic::<SleeperIntrinsic>()])
            .extension_installer(RuntimeExtensionInstaller::new({
                let calls = calls.clone();
                let observations = observations.clone();
                move |realm| {
                    close_motion::install(realm)?;
                    realm.install_native_global_call(
                        "closeCollect",
                        2,
                        RuntimeNativeCall::Dynamic(Arc::new({
                            let calls = calls.clone();
                            let observations = observations.clone();
                            move |ctx: &mut RuntimeNativeCtx<'_>,
                                  args: &[RuntimeValue],
                                  _captures: &[RuntimeValue]| {
                                calls.fetch_add(1, Ordering::SeqCst);
                                let before = ctx.interp_mut().gc_heap().gc_cycle_counts();
                                let children = close_motion::observe_and_collect(ctx, args);
                                let after = ctx.interp_mut().gc_heap().gc_cycle_counts();
                                observations
                                    .lock()
                                    .map_err(|_| NativeError::Error {
                                        message: "close observation poisoned".into(),
                                    })?
                                    .push((before, after, children));
                                Ok(RuntimeValue::undefined())
                            }
                        })),
                    )
                }
            }))
            .build()
            .expect("fromAsync close runtime");
        let close = if getter {
            "get return() { closeCount++; closeCollect(original,alias); throw new SyntaxError('suppressed cleanup getter'); }"
        } else {
            "return() { closeCount++; closeCollect(original,alias); throw new SyntaxError('suppressed cleanup call'); }"
        };
        let source = format!(
            r#"
            const original = {{marker:719}}, alias = original;
            let closeCount = 0;
            const iterator = {{
                [Symbol.asyncIterator]() {{ return this; }},
                next() {{ return Promise.resolve({{value:1,done:false}}); }},
                {close}
            }};
            Array.fromAsync(iterator, () => Promise.reject(original)).catch(error => {{
                console.log((error === original && error === alias) + ':' + error.marker + ':' + closeCount);
                new Sleeper('reaction').failSync();
            }});
        "#
        );
        let error = runtime
            .run_script(
                SourceInput::from_javascript(&source),
                "<fromAsync-close-fatal-reaction>",
            )
            .expect_err("later structural reaction remains an actual drain failure");
        assert_structural(&error);
        let OtterError::Internal { message, .. } = &error else {
            unreachable!()
        };
        assert!(!message.contains("suppressed cleanup"), "{error:?}");
        assert_eq!(capture.snapshot(), ["true:719:1"]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let observations = observations.lock().unwrap();
        let [(before, after, children)] = observations.as_slice() else {
            panic!("one actual collecting return getter/call");
        };
        assert!(after.1 > before.1, "real full GC inside cleanup");
        let children = children
            .as_ref()
            .unwrap_or_else(|error| panic!("{error:?}"));
        let [original, alias] = children.as_slice() else {
            panic!("two actual aliased reasons")
        };
        assert_eq!(original.before, alias.before);
        assert_eq!(original.after, alias.after);
        assert_eq!(
            [
                original.marker_before,
                original.marker_after,
                alias.marker_before,
                alias.marker_after
            ],
            [719.0; 4]
        );
        // The VM proof independently requires actual young movement. Runtime
        // execution also accepts inputs tenured by externally enabled stress.
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generator_body_and_recursive_close_keep_actual_materialization_oom_terminal() {
    const CAP: u64 = 4 * 1024 * 1024;
    for (source, expected) in [
        (
            r#"
            const s = new Sleeper('generator method');
            function* body() {
                try { console.log('native'); s.largeSync(); }
                finally { console.log('body cleanup'); }
            }
            try { body().next(); }
            catch (_) { console.log('caught'); }
            finally { console.log('outer cleanup'); }
        "#,
            vec!["native"],
        ),
        (
            r#"
            const s = new Sleeper('generator native');
            function* body() {
                try { console.log('native'); s.largeSync(); }
                finally { console.log('body cleanup'); }
            }
            const g = body();
            try { g.next.call(g); }
            catch (_) { console.log('caught'); }
            finally { console.log('outer cleanup'); }
        "#,
            vec!["native"],
        ),
        (
            r#"
            const s = new Sleeper('generator close');
            function* body() {
                try { yield 719; }
                finally { console.log('native'); s.largeSync(); console.log('after native'); }
            }
            try {
                for (const value of body().map(value => value)) {
                    console.log('yield:' + value);
                    break;
                }
            } catch (_) { console.log('caught'); }
            finally { console.log('outer cleanup'); }
        "#,
            vec!["yield:719", "native"],
        ),
    ] {
        let capture = LogCapture::new();
        let otter = Otter::builder()
            .max_heap_bytes(CAP)
            .console_sink(capture.clone())
            .global_classes([GlobalClass::from_intrinsic::<SleeperIntrinsic>()])
            .build()
            .expect("capped generator completion runtime");
        let error = otter
            .handle()
            .run_script(
                SourceInput::from_javascript(source),
                "<generator-completed-oom>",
            )
            .await
            .expect_err("completed Error-building OOM leaves the source handler extent");
        assert!(
            matches!(error, OtterError::OutOfMemory { requested_bytes, heap_limit_bytes }
            if requested_bytes > CAP && heap_limit_bytes == CAP),
            "{error:?}"
        );
        assert_eq!(capture.snapshot(), expected);
        let activity = otter.activity_stats();
        assert_eq!(activity.pending_ref_host_ops, 0);
        assert_eq!(activity.pending_unref_host_ops, 0);
    }
}
