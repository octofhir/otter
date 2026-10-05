//! Observable lifecycle lookup and collecting idle-listener regressions.
//!
//! # Contents
//! - Exact getter order, lexical precedence, null/callable guards and TDZ.
//! - Actual listener child relocation followed by timer revival and exit.
//!
//! # Invariants
//! Assertions execute in Rust after the native callback returns. JavaScript
//! operands use the normal call/handle owners; only scalar offsets are retained
//! by the observer. The child is born inside the listener after bootstrap and
//! already owns its marker/self slots before the explicit collection.
//!
//! # See also
//! - `super` implements lifecycle delivery.
//! - Existing `process_timer_teardown` and `fatal_checkpoints` cover teardown.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use crate::{ConsoleLevel, ConsoleSink, Otter, Runtime, RuntimeExtensionInstaller, SourceInput};
use otter_vm::{NativeCall, NativeCtx, NativeError, Value};

fn runtime() -> Runtime {
    Runtime::builder()
        .with_nodejs_modules()
        .build()
        .expect("lifecycle runtime")
}

fn setup(runtime: &mut Runtime, source: &str) {
    runtime
        .run_script(SourceInput::from_javascript(source), "lifecycle-setup.js")
        .expect("lifecycle setup");
}

fn completion(runtime: &mut Runtime, source: &str) -> String {
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "lifecycle-observation.js",
        )
        .expect("lifecycle observation")
        .completion_string()
        .to_owned()
}

#[test]
fn lifecycle_reads_current_globals_members_and_script_lexicals_in_expression_order() {
    let mut changed = runtime();
    setup(
        &mut changed,
        r#"
        globalThis.lifecycleReads = [];
        globalThis.lifecycleGlobalReads = 0;
        const first = {};
        const guard = { get emit() { lifecycleReads.push('guard'); return () => { throw new Error('guard called'); }; } };
        const actual = { get emit() { lifecycleReads.push('callee'); return function (event, code) {
            lifecycleReads.push(String(this === actual) + ':' + event + ':' + code);
        }; } };
        const probe = { get exitCode() { lifecycleReads.push('probe'); return 7; } };
        const final = { get exitCode() { lifecycleReads.push('value'); return 9; } };
        const receivers = [first, guard, actual, probe, final];
        Object.defineProperty(globalThis, 'process', { configurable: true, get() {
            const receiver = receivers[lifecycleGlobalReads++];
            if (!receiver) throw new Error('extra process binding read');
            return receiver;
        } });
    "#,
    );
    assert_eq!(
        changed
            .emit_process_before_exit()
            .expect("changed beforeExit")
            .completion_string(),
        "0"
    );
    assert_eq!(
        completion(
            &mut changed,
            "String(lifecycleGlobalReads) + ':' + lifecycleReads.join(',')"
        ),
        "5:guard,callee,probe,value,true:beforeExit:9"
    );

    let mut exit = runtime();
    setup(
        &mut exit,
        r#"
        globalThis.lifecycleReads = [];
        globalThis.lifecycleGlobalReads = 0;
        const first = {};
        const guard = { get __otterEmitExit() { lifecycleReads.push('guard'); return () => { throw new Error('guard called'); }; } };
        const actual = { get __otterEmitExit() { lifecycleReads.push('callee'); return function (code, failed) {
            lifecycleReads.push(String(this === actual) + ':' + code + ':' + failed);
            return '37';
        }; } };
        const receivers = [first, guard, actual];
        Object.defineProperty(globalThis, 'process', { configurable: true, get() {
            const receiver = receivers[lifecycleGlobalReads++];
            if (!receiver) throw new Error('extra process binding read');
            return receiver;
        } });
    "#,
    );
    assert_eq!(
        exit.emit_process_exit(11, true)
            .expect("changed exit")
            .completion_string(),
        "37"
    );
    assert_eq!(
        completion(
            &mut exit,
            "String(lifecycleGlobalReads) + ':' + lifecycleReads.join(',')"
        ),
        "3:guard,callee,true:11:true"
    );

    let mut lexical = runtime();
    setup(
        &mut lexical,
        r#"
        globalThis.lifecycleSeen = '';
        let process = { exitCode: 23, emit(event, code) { lifecycleSeen = String(this === process) + ':' + event + ':' + code; } };
        Object.defineProperty(globalThis, 'process', { configurable: true, get() { throw new Error('object record used'); } });
    "#,
    );
    lexical
        .emit_process_before_exit()
        .expect("lexical process precedes object record");
    assert_eq!(
        completion(&mut lexical, "lifecycleSeen"),
        "true:beforeExit:23"
    );

    let mut callable = runtime();
    setup(
        &mut callable,
        "globalThis.process = function () {}; process.emit = () => { throw new Error('callable process emitted'); };",
    );
    assert_eq!(
        callable
            .emit_process_before_exit()
            .expect("function is not typeof object")
            .completion_string(),
        "0"
    );
    assert_eq!(
        callable
            .emit_process_exit(13, false)
            .expect("callable exit guard")
            .completion_string(),
        "13"
    );
    setup(
        &mut callable,
        "globalThis.process = new Proxy(function () {}, {}); process.emit = () => { throw new Error('callable proxy emitted'); };",
    );
    assert_eq!(
        callable
            .emit_process_before_exit()
            .expect("callable Proxy keeps typeof function")
            .completion_string(),
        "0"
    );

    let mut deleted = runtime();
    setup(
        &mut deleted,
        r#"
        Object.defineProperty(globalThis, 'process', { configurable: true, get() {
            delete globalThis.process;
            return {};
        } });
    "#,
    );
    let error = deleted
        .emit_process_before_exit()
        .expect_err("the later identifier read became unresolvable");
    assert!(format!("{error:?}").contains("ReferenceError"), "{error:?}");
    assert_eq!(
        deleted
            .emit_process_before_exit()
            .expect("initial typeof suppresses the now-missing name")
            .completion_string(),
        "0"
    );

    setup(
        &mut deleted,
        r#"
        globalThis.lifecycleHasCalls = [];
        const inheritedProcess = { emit() { throw new Error('false has trap still emitted'); } };
        const target = { process: inheritedProcess };
        const inner = new Proxy(target, {
            getOwnPropertyDescriptor(object, key) {
                lifecycleHasCalls.push('descriptor:' + key);
                return Object.getOwnPropertyDescriptor(object, key);
            },
            isExtensible(object) {
                lifecycleHasCalls.push('extensible');
                return Object.isExtensible(object);
            }
        });
        const outer = new Proxy(inner, {
            get(object, key) { return key === 'process' ? inheritedProcess : undefined; },
            has(object, key) { lifecycleHasCalls.push('has:' + key); return false; }
        });
        Object.setPrototypeOf(globalThis, outer);
    "#,
    );
    let error = deleted
        .emit_process_before_exit()
        .expect_err("native-only HasBinding retains Proxy invariant operations");
    assert!(format!("{error:?}").contains("ReferenceError"), "{error:?}");
    assert_eq!(
        completion(&mut deleted, "lifecycleHasCalls.join(',')"),
        "has:process,descriptor:process,extensible"
    );

    let mut null = runtime();
    setup(&mut null, "globalThis.process = null;");
    let error = null
        .emit_process_before_exit()
        .expect_err("null reaches member Get and throws");
    assert!(format!("{error:?}").contains("TypeError"), "{error:?}");

    let mut tdz = runtime();
    tdz.run_script(
        SourceInput::from_javascript(
            "let process = (() => { throw new Error('setup leaves TDZ'); })();",
        ),
        "lifecycle-tdz.js",
    )
    .expect_err("failed initializer leaves an uninitialized declarative binding");
    let error = tdz
        .emit_process_before_exit()
        .expect_err("typeof a TDZ binding still throws");
    assert!(format!("{error:?}").contains("ReferenceError"), "{error:?}");
    assert!(
        !format!("{error:?}").contains("setup leaves TDZ"),
        "fresh lifecycle owns its diagnostic: {error:?}"
    );
}

#[derive(Debug, Default)]
struct Capture(Mutex<Vec<String>>);

impl ConsoleSink for Capture {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        if matches!(level, ConsoleLevel::Log) {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(fields.join(" "));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collecting_before_exit_listener_retains_current_children_and_revives_a_timer() {
    let capture = Arc::new(Capture::default());
    let offsets = Arc::new(Mutex::new(Vec::<u32>::new()));
    let collections = Arc::new(AtomicUsize::new(0));
    let observed = offsets.clone();
    let collected = collections.clone();
    let otter = Otter::builder()
        .console_sink(capture.clone())
        .extension_installer(RuntimeExtensionInstaller::new(move |realm| {
            let observed = observed.clone();
            realm.install_native_global_call(
                "lifecycleOffset",
                1,
                NativeCall::Dynamic(Arc::new(
                    move |_ctx: &mut NativeCtx<'_>,
                          args: &[Value],
                          _captures: &[Value]|
                          -> Result<Value, NativeError> {
                        let child = args
                            .first()
                            .copied()
                            .and_then(Value::as_object)
                            .ok_or_else(|| NativeError::Error {
                                message: "lifecycle observer expected a live child".into(),
                            })?;
                        let offset = child.offset();
                        observed
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(offset);
                        Ok(Value::number_i32(offset as i32))
                    },
                )),
            )?;
            let collected = collected.clone();
            realm.install_native_global_call(
                "lifecycleCollect",
                0,
                NativeCall::Dynamic(Arc::new(
                    move |ctx: &mut NativeCtx<'_>,
                          _args: &[Value],
                          _captures: &[Value]|
                          -> Result<Value, NativeError> {
                        ctx.interp_mut().force_gc().map_err(NativeError::from)?;
                        collected.fetch_add(1, Ordering::SeqCst);
                        Ok(Value::undefined())
                    },
                )),
            )?;
            Ok(())
        }))
        .build()
        .expect("collecting lifecycle runtime");
    let result = otter.run_script(r#"
        globalThis.lifecycleMotionChild = undefined;
        let beforeCount = 0;
        process.on('beforeExit', function (code) {
            if (this !== process) throw new Error('beforeExit receiver');
            console.log('before', code);
            if (++beforeCount === 1) {
                const child = { marker: 17, self: null };
                child.self = child;
                const alias = child;
                lifecycleMotionChild = child;
                const before = lifecycleOffset(child);
                lifecycleCollect();
                const after = lifecycleOffset(child);
                if (before === after || child !== alias || child !== lifecycleMotionChild || child.self !== child || child.marker !== 17) {
                    throw new Error('listener child motion or alias');
                }
                process.exitCode = 6;
                setTimeout(() => { console.log('timer'); }, 1);
            }
        });
        process.on('exit', function (code) {
            if (this !== process || lifecycleMotionChild.self !== lifecycleMotionChild || lifecycleMotionChild.marker !== 17) {
                throw new Error('exit receiver or retained child');
            }
            console.log('exit', code);
        });
        'entry';
    "#).await.expect("collecting listener and timer revival complete");
    assert_eq!(result.completion_string(), "entry");
    assert_eq!(result.exit_code(), 6);
    assert_eq!(collections.load(Ordering::SeqCst), 1);
    let offsets = offsets.lock().expect("owned observations");
    assert_eq!(offsets.len(), 2);
    assert_ne!(
        offsets[0], offsets[1],
        "the already-born listener child actually relocated"
    );
    assert_eq!(
        *capture.0.lock().expect("console capture"),
        ["before 0", "timer", "before 6", "exit 6"]
    );
    assert_eq!(otter.activity_stats().pending_ref_timers, 0);
    assert_eq!(otter.activity_stats().pending_unref_timers, 0);
}
