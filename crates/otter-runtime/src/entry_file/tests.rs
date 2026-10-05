//! Actual CommonJS source identity and worker-entry readiness proofs.
//!
//! # Contents
//! - Escaped entry callbacks and failed module records keep defining sources.
//! - Immediately queued worker messages execute after a CommonJS entry in FIFO.
//!
//! # Invariants
//! - Tests call the ordinary file, scoped native and worker owners. Source
//!   observations are owned data captured inside native callbacks; assertions
//!   stay outside their ABI entry.
//! - A real full collection precedes the escaped callback's cross-chunk call.
//!   Rollback never executes accessor or Proxy deletion code.
//!
//! # See also
//! - `crate::entry_file` and `crate::worker`.

use super::*;
use std::sync::{Arc, Mutex};

#[test]
fn commonjs_callbacks_and_throws_keep_their_exact_defining_source() {
    let dir = tempfile::tempdir().expect("entry directory");
    let entry = dir.path().join("source-owned.cjs");
    std::fs::write(
        &entry,
        r#"
        globalThis.savedCjsCallback = function savedCjsCallback() {
            captureCjsSource();
            return new Error("saved CJS callback").stack;
        };
        globalThis.firstCjsStack = savedCjsCallback();
        "#,
    )
    .expect("entry source");
    let expected_source = std::fs::canonicalize(&entry)
        .expect("canonical source")
        .to_string_lossy()
        .into_owned();
    let observations = Arc::new(Mutex::new(Vec::<(Option<String>, String)>::new()));
    let observed = observations.clone();
    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_nodejs_modules()
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .expect("CommonJS runtime");
    runtime
        .install_native_global_call(
            "captureCjsSource",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(move |ctx, _args, _state| {
                let source = ctx
                    .execution_context()
                    .map(|context| context.module_name().to_owned());
                let stack = ctx.execution_context().map_or_else(String::new, |context| {
                    ctx.capture_call_sites_json(context, 0, usize::MAX)
                });
                observed
                    .lock()
                    .map_err(|_| otter_vm::NativeError::InvalidOperand)?
                    .push((source, stack));
                Ok(otter_vm::Value::undefined())
            })),
        )
        .expect("source observer");
    runtime.run_file(&entry).expect("CommonJS source entry");
    let before = runtime.heap_stats().gc_cycles;
    runtime.force_gc().expect("collect escaped callback");
    assert!(runtime.heap_stats().gc_cycles > before);
    let stack = runtime
        .run_script(
            SourceInput::from_javascript("savedCjsCallback();"),
            "other-source.js",
        )
        .expect("escaped callback from another chunk")
        .completion_string()
        .to_owned();
    assert!(stack.contains(&expected_source), "{stack}");
    let observations = observations.lock().expect("source observations");
    assert_eq!(observations.len(), 2);
    for (source, stack) in observations.iter() {
        assert_eq!(source.as_deref(), Some(expected_source.as_str()));
        assert!(stack.contains(&expected_source), "{stack}");
        assert!(stack.contains("savedCjsCallback"), "{stack}");
        assert!(!stack.contains("<commonjs-root>"), "{stack}");
    }
    drop(observations);

    // Both user-visible cache variants must leave the original throw intact
    // without evaluating a rollback accessor or Proxy deleteProperty trap.
    for proxy in [false, true] {
        let name = if proxy {
            "proxy-failure.cjs"
        } else {
            "accessor-failure.cjs"
        };
        let failing_entry = dir.path().join(name);
        runtime
            .run_script(
                SourceInput::from_javascript(
                    "globalThis.rollbackReads = 0; globalThis.__otterRequireCache = Object.create(null);",
                ),
                "cache-preparation.js",
            )
            .expect("cache variant");
        std::fs::write(
            &failing_entry,
            if proxy {
                r#"
                globalThis.rollbackCache = require.cache;
                globalThis.rollbackFilename = __filename;
                globalThis.__otterRequireCache = new Proxy(require.cache, {
                    deleteProperty() { rollbackReads++; throw new Error("cleanup trap"); }
                });
                function failCjsSource() { throw new Error("original CJS failure"); }
                failCjsSource();
                "#
            } else {
                r#"
                globalThis.rollbackCache = require.cache;
                globalThis.rollbackFilename = __filename;
                Object.defineProperty(require.cache, __filename, {
                    get() { rollbackReads++; throw new Error("cleanup accessor"); },
                    configurable: true
                });
                function failCjsSource() { throw new Error("original CJS failure"); }
                failCjsSource();
                "#
            },
        )
        .expect("failing entry");
        let failure = runtime
            .run_file(&failing_entry)
            .expect_err("original entry throw");
        let OtterError::Runtime { diagnostic } = failure else {
            panic!("expected defining-source runtime throw, got {failure:?}");
        };
        let expected = std::fs::canonicalize(&failing_entry)
            .expect("failure source")
            .to_string_lossy()
            .into_owned();
        assert_eq!(diagnostic.code, DiagnosticCode::Uncaught.as_str());
        assert!(
            diagnostic.message.contains("original CJS failure"),
            "{diagnostic:?}"
        );
        assert!(
            diagnostic.frames.iter().any(|frame| {
                frame.module == expected
                    && frame.function == "failCjsSource"
                    && frame.source_position.is_some()
            }),
            "{diagnostic:?}"
        );
        assert!(
            !diagnostic
                .frames
                .iter()
                .any(|frame| frame.module == "<commonjs-root>")
        );
        let reads = runtime
            .run_script(
                SourceInput::from_javascript(
                    "JSON.stringify([rollbackReads, Object.prototype.hasOwnProperty.call(rollbackCache, rollbackFilename)]);",
                ),
                "after-failure.js",
            )
            .expect("read cleanup count")
            .completion_string()
            .to_owned();
        assert_eq!(reads, if proxy { "[0,false]" } else { "[0,true]" });
    }

    runtime
        .run_script(
            SourceInput::from_javascript("globalThis.__otterRequireCache = Object.create(null);"),
            "completed-cache-preparation.js",
        )
        .expect("ordinary cache for completed entry");

    // A completed native control failure inside a real CommonJS body must
    // retain its frame/detail and bypass both uncaught listeners and jobs.
    runtime
        .install_native_global_call(
            "fatalCjsCompleted",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(|_ctx, _args, _state| {
                Err(otter_vm::NativeError::BudgetExceeded {
                    reason: "completed CommonJS budget cause".to_owned(),
                })
            })),
        )
        .expect("completed failure native");
    let fatal_entry = dir.path().join("completed-failure.cjs");
    std::fs::write(
        &fatal_entry,
        r#"
        globalThis.cjsUnhandledCalls = 0;
        globalThis.cjsCheckpointCalls = 0;
        process.on("uncaughtException", () => { cjsUnhandledCalls++; });
        Promise.resolve().then(() => { cjsCheckpointCalls++; });
        function failCjsCompleted() { fatalCjsCompleted(); }
        failCjsCompleted();
        "#,
    )
    .expect("completed failure entry");
    let failure = runtime
        .run_file(&fatal_entry)
        .expect_err("completed control failure");
    let OtterError::Runtime { diagnostic } = failure else {
        panic!("expected owned completed failure, got {failure:?}");
    };
    assert_eq!(diagnostic.code, DiagnosticCode::BudgetExceeded.as_str());
    assert!(
        diagnostic
            .message
            .contains("completed CommonJS budget cause")
    );
    let expected = std::fs::canonicalize(&fatal_entry)
        .expect("completed source")
        .to_string_lossy()
        .into_owned();
    assert!(
        diagnostic.frames.iter().any(|frame| {
            frame.module == expected
                && frame.function == "failCjsCompleted"
                && frame.source_position.is_some()
        }),
        "{diagnostic:?}"
    );
    let counters = otter_vm::NativeCtx::with_host_context(
        &mut runtime.interp,
        otter_vm::NativeCallInfo::default_call(),
        None,
        |ctx| {
            ctx.scope(|mut scope| -> Result<_, otter_vm::NativeError> {
                let unhandled = scope
                    .global("cjsUnhandledCalls")
                    .ok_or(otter_vm::NativeError::InvalidOperand)?;
                let checkpoint = scope
                    .global("cjsCheckpointCalls")
                    .ok_or(otter_vm::NativeError::InvalidOperand)?;
                Ok((
                    scope.number_value(unhandled)?,
                    scope.number_value(checkpoint)?,
                ))
            })
        },
    )
    .expect("read counters without checkpoint");
    assert_eq!(counters, (0.0, 0.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commonjs_worker_entry_publishes_readiness_before_fifo_messages() {
    let dir = tempfile::tempdir().expect("worker directory");
    let worker = dir.path().join("source-worker.cjs");
    std::fs::write(
        &worker,
        r#"
        const entryStack = new Error("worker entry").stack;
        let count = 0;
        globalThis.onmessage = function workerSourceMessage(event) {
            const callbackStack = new Error("worker callback").stack;
            postMessage([event.data, ++count,
                entryStack.includes("source-worker.cjs"),
                callbackStack.includes("source-worker.cjs")]);
        };
        "#,
    )
    .expect("CommonJS worker source");
    let entry = dir.path().join("parent.cjs");
    std::fs::write(
        &entry,
        format!(
            r#"
            globalThis.workerSourceReplies = [];
            const worker = new Worker({:?});
            globalThis.activeSourceWorker = worker;
            worker.onmessage = parentWorkerMessage;
            worker.onerror = function failedWorker(event) {{
                worker.terminate();
                throw new Error("worker failed: " + event.message);
            }};
            worker.postMessage("first");
            worker.postMessage("second");
            "#,
            worker.to_string_lossy(),
        ),
    )
    .expect("parent entry");
    let otter = Otter::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_nodejs_modules()
        .build()
        .expect("worker runtime");
    // The parent callback belongs to an independent admitted script chunk;
    // delivery cannot rely on retaining the CommonJS constructor's context.
    otter
        .run_script_source(
            SourceInput::from_javascript(
                r#"
                globalThis.parentWorkerMessage = function parentWorkerMessage(event) {
                    const definingSource = new Error("parent callback").stack;
                    workerSourceReplies.push([
                        event.data[0], event.data[1], event.data[2], event.data[3],
                        definingSource.includes("parent-handler.js")
                    ]);
                    if (workerSourceReplies.length === 2) activeSourceWorker.terminate();
                };
                "#,
            ),
            "parent-handler.js",
        )
        .await
        .expect("independent parent callback source");
    tokio::time::timeout(std::time::Duration::from_secs(10), otter.run_file(&entry))
        .await
        .expect("worker FIFO completion deadline")
        .expect("worker entry and messages");
    let result = otter
        .run_script_source(
            SourceInput::from_javascript("JSON.stringify(workerSourceReplies);"),
            "worker-result.js",
        )
        .await
        .expect("worker observations");
    assert_eq!(
        result.completion_string(),
        r#"[["first",1,true,true,true],["second",2,true,true,true]]"#
    );
}
