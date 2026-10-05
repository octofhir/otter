//! Actual Runtime reporter, source, FIFO and moving-exception proofs.
//!
//! # Contents
//! - Deliberate later turns report retained jobs in their original source.
//! - Collecting reporters preserve aliases or propagate exact Exit.
//! - Native-only timer admission restores realm and moved ambient async state.
//!
//! # Invariants
//! Observations are owned scalars/text recorded inside callbacks; assertions run
//! after the native ABI returns. All VM values retained through collection use
//! production scopes or persistent roots. Scheduler tokens remain owned data.
//!
//! # See also
//! - crate::uncaught and crate::timer_delivery own the tested extents.

use crate::{Runtime, SourceInput};
use otter_vm::{NativeCallInfo, NativeCtx, NativeError, Value};
use std::sync::{Arc, Mutex};

#[derive(Debug, PartialEq)]
struct Seen {
    source: Option<String>,
    realm: u32,
    moved: bool,
    aliases: bool,
}

#[derive(Clone, Copy)]
enum Reporter {
    Handle,
    Catchable,
    Exit,
}

fn install_reporter(runtime: &mut Runtime, observed: Arc<Mutex<Vec<Seen>>>, mode: Reporter) {
    NativeCtx::with_host_context(
        &mut runtime.interp,
        NativeCallInfo::default_call(),
        None,
        |ctx| {
            ctx.scope(|mut scope| -> Result<(), NativeError> {
                let process = scope.bare_object()?;
                let capture =
                    scope.native_closure("report-original", 1, &[], move |ctx, args, _| {
                        let source = ctx
                            .execution_context()
                            .map(|source| source.module_name().to_owned());
                        let realm = ctx.interp_mut().active_host_realm_id();
                        ctx.scope(|mut scope| {
                            let thrown = scope.argument(args, 0);
                            let moved = move_rooted(&mut scope, thrown)?;
                            let original = scope
                                .global("original")
                                .ok_or(NativeError::InvalidOperand)?;
                            let link = scope.get(thrown, "self")?;
                            observed
                                .lock()
                                .map_err(|_| NativeError::InvalidOperand)?
                                .push(Seen {
                                    source,
                                    realm,
                                    moved,
                                    aliases: scope.strict_equals(thrown, original)
                                        && scope.strict_equals(thrown, link),
                                });
                            match mode {
                                Reporter::Handle => {
                                    let followup = scope.native_closure(
                                        "report-followup",
                                        0,
                                        &[],
                                        |ctx, _, _| {
                                            ctx.scope(|mut scope| {
                                                let global = scope.global_this();
                                                let value = scope.number(719.0);
                                                scope.define(
                                                    global,
                                                    "followup",
                                                    value,
                                                    otter_vm::object::PropertyFlags::data_default(),
                                                )?;
                                                Ok(Value::undefined())
                                            })
                                        },
                                    )?;
                                    scope.queue_microtask(followup, &[])?;
                                    Ok(Value::undefined())
                                }
                                Reporter::Catchable => Err(NativeError::SpecError {
                                    kind: otter_vm::ErrorKind::SyntaxError,
                                    message: "subordinate reporter failure".into(),
                                }),
                                Reporter::Exit => Err(NativeError::Exit { code: 27 }),
                            }
                        })
                    })?;
                scope.define(
                    process,
                    crate::process_control::CAPTURE_SLOT,
                    capture,
                    otter_vm::object::PropertyFlags::data_default(),
                )?;
                let global = scope.global_this();
                scope.set(global, "process", process)?;
                Ok(())
            })
        },
    )
    .expect("rooted reporter installation");
}

fn move_rooted(
    scope: &mut otter_vm::NativeScope<'_, '_>,
    value: otter_vm::Local<'_>,
) -> Result<bool, NativeError> {
    let root = scope.persistent_root_insert(value);
    let ambient = scope.async_context();
    let observed = scope.with_async_context(ambient, |ctx| {
        let before = ctx
            .persistent_root_get(root)
            .and_then(|value| value.as_object())
            .ok_or(NativeError::InvalidOperand)?
            .offset();
        let stats = ctx.interp_mut().gc_stats_snapshot();
        ctx.interp_mut().force_gc().map_err(NativeError::from)?;
        let after = ctx
            .persistent_root_get(root)
            .and_then(|value| value.as_object())
            .ok_or(NativeError::InvalidOperand)?
            .offset();
        Ok(before != after
            && ctx.interp_mut().gc_stats_snapshot().minor_gc_cycles > stats.minor_gc_cycles)
    });
    // Cleanup runs even if the real collector returns an error. The original
    // Local remains traced; no raw copy or finished scope survives collection.
    let retained = scope
        .take_persistent_root(root)
        .ok_or(NativeError::InvalidOperand)?;
    if !scope.strict_equals(retained, value) {
        return Err(NativeError::InvalidOperand);
    }
    observed
}

#[test]
fn retained_job_reports_original_source_after_foreign_turn_and_handler_followup_is_fifo() {
    let mut runtime = Runtime::builder()
        .build()
        .expect("Runtime reporter fixture");
    runtime.interp.gc_heap_mut().set_gc_stress(0, true);
    let observed = Arc::new(Mutex::new(Vec::new()));
    install_reporter(&mut runtime, observed.clone(), Reporter::Handle);
    runtime
        .install_native_global("stopOriginalTurn", 0, |_, _| {
            Err(NativeError::InvalidOperand)
        })
        .unwrap();
    runtime.run_script(SourceInput::from_javascript(
        "globalThis.original = {marker:317}; original.self = original; queueMicrotask(() => { throw original; }); stopOriginalTurn();"
    ), "original-job.js").expect_err("structural script failure leaves queued original job");
    assert!(observed.lock().unwrap().is_empty());
    let outcome = runtime
        .run_script(
            SourceInput::from_javascript("globalThis.foreignRan = true; 41;"),
            "foreign-turn.js",
        )
        .unwrap();
    assert_eq!(outcome.completion_string(), "41");
    let rows = observed.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0]
            .source
            .as_deref()
            .is_some_and(|name| name.contains("original-job.js")),
        "job source must not become the foreign drain source: {:?}",
        rows[0]
    );
    assert_eq!(rows[0].realm, 0);
    assert!(rows[0].moved && rows[0].aliases);
    drop(rows);
    let result = runtime
        .run_script(
            SourceInput::from_javascript("foreignRan + ':' + followup"),
            "read-followup.js",
        )
        .unwrap();
    assert_eq!(result.completion_string(), "true:719");
    assert!(!runtime.microtask_stats().pending);
}

#[test]
fn catchable_reporter_failure_restores_actual_moved_original_throw() {
    let mut runtime = Runtime::builder().build().unwrap();
    runtime.interp.gc_heap_mut().set_gc_stress(0, true);
    let observed = Arc::new(Mutex::new(Vec::new()));
    install_reporter(&mut runtime, observed.clone(), Reporter::Catchable);
    let error = runtime.run_script(SourceInput::from_javascript(
        "globalThis.original = {marker:719, cause: {message: 'original cause 719'}}; original.self = original; queueMicrotask(() => { throw original; });"
    ), "unhandled-original.js").expect_err("subordinate reporter cannot handle the original");
    // The direct Runtime turn consumes the restored pending throw while it
    // projects the diagnostic: its cause chain is read from the pending value
    // after the reporter's collection moved it. The reporter's own SyntaxError
    // has no cause and would have named the subordinate failure instead.
    let crate::OtterError::Runtime { diagnostic } = error else {
        panic!("original uncaught throw keeps its runtime diagnostic: {error:?}");
    };
    assert!(
        !diagnostic.message.contains("subordinate reporter failure"),
        "{diagnostic:?}"
    );
    assert_eq!(
        diagnostic
            .cause
            .as_ref()
            .map(|cause| cause.message.as_str()),
        Some("Error: original cause 719"),
        "{diagnostic:?}"
    );
    assert_eq!(
        runtime
            .run_script(
                SourceInput::from_javascript("original.self === original && original.marker"),
                "original-alias-report.js",
            )
            .expect("original alias survives the failed turn")
            .completion_string(),
        "719"
    );
    let rows = observed.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].moved && rows[0].aliases);
}

#[test]
fn reporter_exit_stops_current_checkpoint_and_only_later_turn_resumes_old_followup() {
    let mut runtime = Runtime::builder().build().unwrap();
    runtime.interp.gc_heap_mut().set_gc_stress(0, true);
    let observed = Arc::new(Mutex::new(Vec::new()));
    install_reporter(&mut runtime, observed.clone(), Reporter::Exit);
    let result = runtime.run_script(SourceInput::from_javascript(
        "globalThis.original = {}; original.self = original; globalThis.late = 0; queueMicrotask(() => { throw original; }); queueMicrotask(() => { late = 43; });"
    ), "exit-original.js").expect("exact exit completion");
    assert_eq!(result.exit_code(), 27);
    assert!(runtime.microtask_stats().pending);
    let later = runtime
        .run_script(
            SourceInput::from_javascript("let beforeOldJob = late; beforeOldJob;"),
            "deliberate-later-turn.js",
        )
        .unwrap();
    assert_eq!(later.completion_string(), "0");
    let late = runtime
        .run_script(SourceInput::from_javascript("late"), "read-late.js")
        .unwrap();
    assert_eq!(late.completion_string(), "43");
    let rows = observed.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].moved && rows[0].aliases);
}

struct Scheduler;
impl otter_vm::TimerScheduler for Scheduler {
    fn admit(&self, _: bool) -> Result<otter_vm::TimerAdmission, String> {
        Ok(otter_vm::TimerAdmission::new(Box::new(())))
    }
    fn schedule(&self, _: otter_vm::TimerAdmission, _: u64, _: Option<u64>) -> Result<u64, String> {
        Ok(317)
    }
    fn cancel(&self, _: u64) -> bool {
        true
    }
    fn set_ref(&self, _: u64, _: bool) -> bool {
        true
    }
}

#[test]
fn native_only_timer_uses_origin_realm_and_restores_moved_ambient_async_before_exit() {
    let mut runtime = Runtime::builder().build().unwrap();
    runtime.interp.gc_heap_mut().set_gc_stress(0, true);
    runtime.install_timer_scheduler(Arc::new(Scheduler));
    let realm = runtime.interp.create_host_realm().unwrap();
    let realm_id = runtime
        .interp
        .with_host_realm(realm, |vm| Ok(vm.active_host_realm_id()))
        .unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let root = NativeCtx::with_host_context(
        &mut runtime.interp,
        NativeCallInfo::default_call(),
        None,
        |ctx| {
            ctx.scope(|mut scope| -> Result<_, NativeError> {
                let ambient = scope.object()?;
                scope.set_async_context(ambient);
                Ok(scope.persistent_root_insert(ambient))
            })
        },
    )
    .unwrap();
    runtime
        .interp
        .with_host_realm(realm, |vm| {
            Ok(NativeCtx::with_host_context(
                vm,
                NativeCallInfo::default_call(),
                None,
                |ctx| {
                    ctx.scope(|mut scope| -> Result<(), NativeError> {
                        let origin = scope.object()?;
                        let marker = scope.number(719.0);
                        scope.define(
                            origin,
                            "marker",
                            marker,
                            otter_vm::object::PropertyFlags::data_default(),
                        )?;
                        scope.set_async_context(origin);
                        let callback = scope.native_closure(
                            "native-only-timer",
                            0,
                            &[],
                            move |ctx, _, _| {
                                let source =
                                    ctx.execution_context().map(|c| c.module_name().to_owned());
                                let realm = ctx.interp_mut().active_host_realm_id();
                                ctx.scope(|mut scope| {
                                    let async_value = scope.async_context();
                                    let moved = move_rooted(&mut scope, async_value)?;
                                    let marker = scope.get(async_value, "marker")?;
                                    sink.lock().map_err(|_| NativeError::InvalidOperand)?.push(
                                        Seen {
                                            source,
                                            realm,
                                            moved,
                                            aliases: scope.number_value(marker).ok() == Some(719.0),
                                        },
                                    );
                                    Err(NativeError::Exit { code: 27 })
                                })
                            },
                        )?;
                        assert_eq!(scope.schedule_interval(callback, 1)?, 317);
                        Ok(())
                    })
                },
            ))
        })
        .unwrap()
        .unwrap();
    // Ambient values are explicitly chosen after scheduling, independently of
    // the entry's captured origin async context.
    runtime
        .interp
        .set_async_context(runtime.interp.persistent_root_get(root).unwrap());
    let result = runtime.fire_timer(317).expect("timer exact exit");
    assert!(matches!(
        result,
        crate::TimerFireOutcome::Fired { repeat: true }
    ));
    assert_eq!(runtime.take_pending_exit_code(), Some(27));
    assert_eq!(runtime.interp.active_host_realm_id(), 0);
    assert_eq!(
        runtime.interp.async_context(),
        runtime.interp.persistent_root_get(root).unwrap()
    );
    assert_eq!(
        observed.lock().unwrap().as_slice(),
        &[Seen {
            source: None,
            realm: realm_id,
            moved: true,
            aliases: true
        }]
    );
    assert!(runtime.interp.cancel_timer(317));
    runtime.interp.persistent_root_remove(root);
}
