//! Additional-realm script completions through actual collecting checkpoints.
//!
//! # Contents
//! - Object and dynamic string completion relocation with retained aliases.
//! - Persistent-root publication during the checkpoint and release afterward.
//! - Catchable checkpoint failure retaining its original source diagnostic.
//!
//! # Invariants
//! - Fresh completions are constructed through native handle scopes; only the
//!   existing traced globals and completion owner retain them afterward.
//! - Each queued native callback records exact scalar offsets around real GC.
//! - Nested scopes finish one rooted value into an immediate scalar observation;
//!   no raw moving payload survives an allocating call.
//! - Old space is nonmoving, so observed relocation proves young evacuation.
//! - No fixture assertion or panic unwinds through the native host ABI.
//!
//! # See also
//! - `otter_runtime::realm` releases the completion root on every checkpoint outcome.

use std::sync::{Arc, Mutex};

use otter_runtime::{
    JitSelection, OtterError, Runtime, RuntimeExtensionInstaller, RuntimeNativeCall,
    RuntimeNativeCtx, RuntimeNativeError, RuntimeValue, SourceInput,
};

const STRING_MARKER: &str = "additional realm completion: actual young string 🙂";
const OBJECT_MARKER: f64 = 813.0;

#[derive(Clone, Copy, Debug)]
enum CompletionKind {
    Object,
    String,
}

#[derive(Clone, Debug)]
struct Payload {
    offset: u32,
    alias_offset: u32,
    self_offset: Option<u32>,
    marker: String,
}

#[derive(Clone, Debug)]
struct Motion {
    before: Payload,
    after: Payload,
    minor_before: u64,
    minor_after: u64,
    persistent_before: u64,
    persistent_after: u64,
}

#[derive(Default)]
struct Records {
    baseline: Option<u64>,
    motions: Vec<Result<Motion, String>>,
}

fn failure(message: &str) -> RuntimeNativeError {
    RuntimeNativeError::Error {
        message: message.to_owned(),
    }
}

fn persistent_slots(census: &otter_vm::root_census::RootCensus) -> u64 {
    census
        .sources
        .iter()
        .find(|source| source.name == "persistent_roots")
        .map_or(0, |source| source.slots)
}

fn observe(
    ctx: &mut RuntimeNativeCtx<'_>,
    kind: CompletionKind,
) -> Result<Payload, RuntimeNativeError> {
    ctx.scope(|mut scope| {
        let value = scope
            .global("realmCompletionValue")
            .ok_or_else(|| failure("missing realm completion value"))?;
        let alias = scope
            .global("realmCompletionAlias")
            .ok_or_else(|| failure("missing realm completion alias"))?;
        let offset = |value: RuntimeValue| match kind {
            CompletionKind::Object => value.as_object().map(|object| object.offset()),
            CompletionKind::String => value.as_string_gc().map(|string| string.offset()),
        };
        let before_value = scope
            .scope(|child| offset(child.finish(value)))
            .ok_or_else(|| failure("completion has the wrong heap payload kind"))?;
        let alias_offset = scope
            .scope(|child| offset(child.finish(alias)))
            .ok_or_else(|| failure("completion alias has the wrong heap payload kind"))?;
        let (self_offset, marker) = match kind {
            CompletionKind::Object => {
                let own_self = scope.get(value, "self")?;
                let own_marker = scope.get(value, "marker")?;
                (
                    Some(
                        scope
                            .scope(|child| offset(child.finish(own_self)))
                            .ok_or_else(|| failure("missing completion self alias"))?,
                    ),
                    scope.number_value(own_marker)?.to_string(),
                )
            }
            CompletionKind::String => (None, scope.string_value(value)?),
        };
        Ok(Payload {
            offset: before_value,
            alias_offset,
            self_offset,
            marker,
        })
    })
}

fn installer(kind: CompletionKind, records: Arc<Mutex<Records>>) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        realm.install_native_global_call(
            "realmCompletionReset",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                |ctx: &mut RuntimeNativeCtx<'_>,
                 _args: &[RuntimeValue],
                 _state: &[RuntimeValue]| {
                    // Exact motion belongs to the queued collection, not ambient
                    // stress during construction of the completion or callback.
                    ctx.interp_mut().gc_heap_mut().set_gc_stress(0, false);
                    Ok(RuntimeValue::undefined())
                },
            )),
        )?;
        let baseline_records = records.clone();
        realm.install_native_global_call(
            "realmCompletionBaseline",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      _args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    let slots = persistent_slots(&ctx.interp_mut().root_census());
                    baseline_records
                        .lock()
                        .map_err(|_| failure("completion baseline recorder poisoned"))?
                        .baseline = Some(slots);
                    Ok(RuntimeValue::undefined())
                },
            )),
        )?;
        realm.install_native_global_call(
            "realmCompletionFresh",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      _args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    ctx.scope(|mut scope| match kind {
                        CompletionKind::Object => {
                            let value = scope.object()?;
                            let marker = scope.number(OBJECT_MARKER);
                            scope.define(
                                value,
                                "marker",
                                marker,
                                otter_vm::object::PropertyFlags::default(),
                            )?;
                            scope.define(
                                value,
                                "self",
                                value,
                                otter_vm::object::PropertyFlags::default(),
                            )?;
                            Ok(scope.finish(value))
                        }
                        CompletionKind::String => {
                            let value = scope.string(STRING_MARKER)?;
                            Ok(scope.finish(value))
                        }
                    })
                },
            )),
        )?;
        let motion_records = records.clone();
        realm.install_native_global_call(
            "realmCompletionMove",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      _args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    let result = (|| {
                        let before = observe(ctx, kind)?;
                        let stats = ctx.interp_mut().gc_stats_snapshot();
                        let persistent_before = persistent_slots(&ctx.interp_mut().root_census());
                        ctx.interp_mut().force_gc().map_err(|error| {
                            otter_vm::native_function::vm_to_native_error(
                                ctx.interp_mut(),
                                error.into(),
                                "realm completion collection",
                            )
                        })?;
                        let after = observe(ctx, kind)?;
                        let after_stats = ctx.interp_mut().gc_stats_snapshot();
                        Ok::<_, RuntimeNativeError>(Motion {
                            before,
                            after,
                            minor_before: stats.minor_gc_cycles,
                            minor_after: after_stats.minor_gc_cycles,
                            persistent_before,
                            persistent_after: persistent_slots(&ctx.interp_mut().root_census()),
                        })
                    })();
                    motion_records
                        .lock()
                        .map_err(|_| failure("completion motion recorder poisoned"))?
                        .motions
                        .push(result.as_ref().cloned().map_err(ToString::to_string));
                    result.map(|_| RuntimeValue::undefined())
                },
            )),
        )
    })
}

fn source(fail_checkpoint: bool) -> SourceInput {
    let failure = if fail_checkpoint {
        "throw new Error('realm checkpoint original failure');"
    } else {
        ""
    };
    SourceInput::from_javascript(format!(
        r#"
realmCompletionReset();
globalThis.realmCompletionValue = undefined;
globalThis.realmCompletionAlias = undefined;
queueMicrotask(function realmCompletionCheckpoint() {{
    realmCompletionMove();
    {failure}
}});
realmCompletionBaseline();
realmCompletionValue = realmCompletionFresh();
realmCompletionAlias = realmCompletionValue;
realmCompletionValue;
"#,
    ))
}

fn assert_motion(runtime: &Runtime, records: &Arc<Mutex<Records>>, kind: CompletionKind) {
    let records = records.lock().expect("owned completion observations");
    let baseline = records
        .baseline
        .expect("baseline before completion existed");
    assert_eq!(records.motions.len(), 1, "exact queued callback count");
    let motion = records.motions[0]
        .as_ref()
        .expect("panic-free real collection");
    assert!(
        motion.minor_after > motion.minor_before,
        "actual young collection"
    );
    assert_ne!(
        motion.before.offset, motion.after.offset,
        "the exact completion moved"
    );
    for payload in [&motion.before, &motion.after] {
        assert_eq!(
            payload.offset, payload.alias_offset,
            "global alias remains exact"
        );
        match kind {
            CompletionKind::Object => {
                assert_eq!(
                    payload.self_offset,
                    Some(payload.offset),
                    "heap self alias rewritten"
                );
                assert_eq!(payload.marker, OBJECT_MARKER.to_string());
            }
            CompletionKind::String => {
                assert_eq!(payload.self_offset, None);
                assert_eq!(payload.marker, STRING_MARKER);
            }
        }
    }
    assert_eq!(
        motion.persistent_before,
        baseline + 1,
        "completion owner published before drain"
    );
    assert_eq!(
        motion.persistent_after,
        baseline + 1,
        "same root survives collecting checkpoint"
    );
    assert_eq!(
        persistent_slots(&runtime.root_census()),
        baseline,
        "root released after checkpoint outcome"
    );
}

#[test]
fn additional_realm_object_completion_moves_with_its_aliases_during_checkpoint() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::ProductionTiered,
    ] {
        let records = Arc::new(Mutex::new(Records::default()));
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .extension_installer(installer(CompletionKind::Object, records.clone()))
            .build()
            .expect("completion runtime");
        let realm = runtime.create_realm().expect("additional realm");
        let result = runtime
            .run_script_in_realm(realm, source(false), "realm-object-completion.js")
            .expect("moved object completion renders from its current root");
        assert_eq!(result.completion_string(), "[object Object]");
        assert_motion(&runtime, &records, CompletionKind::Object);
        let aliases = runtime.run_script_in_realm(realm,
            SourceInput::from_javascript("realmCompletionValue === realmCompletionAlias && realmCompletionValue.self === realmCompletionValue && realmCompletionValue.marker === 813"),
            "realm-object-aliases.js").expect("retained aliases in the source realm");
        assert_eq!(aliases.completion_string(), "true");
    }
}

#[test]
fn additional_realm_dynamic_string_completion_renders_after_real_relocation() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::ProductionTiered,
    ] {
        let records = Arc::new(Mutex::new(Records::default()));
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .extension_installer(installer(CompletionKind::String, records.clone()))
            .build()
            .expect("completion runtime");
        let realm = runtime.create_realm().expect("additional realm");
        let result = runtime
            .run_script_in_realm(realm, source(false), "realm-string-completion.js")
            .expect("moved string completion renders from its current root");
        assert_eq!(result.completion_string(), STRING_MARKER);
        assert_motion(&runtime, &records, CompletionKind::String);
        let retained = runtime.run_script_in_realm(realm,
            SourceInput::from_javascript("realmCompletionValue === realmCompletionAlias ? realmCompletionValue : 'lost alias'"),
            "realm-string-aliases.js").expect("retained string aliases");
        assert_eq!(retained.completion_string(), STRING_MARKER);
    }
}

#[test]
fn additional_realm_failing_checkpoint_releases_the_moved_completion_root() {
    let records = Arc::new(Mutex::new(Records::default()));
    let mut runtime = Runtime::builder()
        .extension_installer(installer(CompletionKind::Object, records.clone()))
        .build()
        .expect("completion runtime");
    let realm = runtime.create_realm().expect("additional realm");
    let error = runtime
        .run_script_in_realm(realm, source(true), "realm-failing-checkpoint.js")
        .expect_err("queued original failure remains primary");
    let OtterError::Runtime { diagnostic } = error else {
        panic!("checkpoint must retain its catchable runtime error: {error:?}");
    };
    assert!(
        diagnostic
            .message
            .contains("realm checkpoint original failure"),
        "{diagnostic:?}"
    );
    assert!(
        diagnostic
            .frames
            .iter()
            .any(|frame| frame.function == "realmCompletionCheckpoint"
                && frame.module == "realm-failing-checkpoint.js"),
        "original queued source frame: {diagnostic:?}"
    );
    assert_motion(&runtime, &records, CompletionKind::Object);
    let recovered = runtime.run_script_in_realm(realm,
        SourceInput::from_javascript("realmCompletionValue.self === realmCompletionAlias && realmCompletionValue.marker === 813"),
        "realm-after-checkpoint-error.js").expect("realm remains reusable after failure");
    assert_eq!(recovered.completion_string(), "true");
}
