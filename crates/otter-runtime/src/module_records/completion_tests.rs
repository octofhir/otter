//! Actual hosted installers and dynamic imports in default/additional realms.
//!
//! # Contents
//! - Shared record/cache publication, real collecting callbacks and CJS namespaces.
//! - Original throw/cause/source, realm-native class, completed fatal and cap refusal.
//!
//! # Invariants
//! - Programs enter genuine Runtime script/module owners and dynamic-import tokens.
//! - Only owned observations leave native callbacks; assertions run in Rust tests.
//! - Young receiver motion is observed before/after the actual collection, with
//!   aliases and payload verified by the continued source callback.
//! - Additional realms run their own configured globals/installers. No default
//!   process singleton, copied namespace or synthetic execution context is used.
//!
//! # See also
//! - `super` owns each environment's actual linked initializer context.
//! - `crate::hosted_completion` owns active-realm failure projection.
//! - `crate::module_records` owns the one installer and module environment table.

use crate::{
    CapabilitySet, DiagnosticCode, HostedModule, JitSelection, NativeError, OtterError, Runtime,
    RuntimeExtensionInstaller, RuntimeLocal, RuntimeNativeCall, RuntimeNativeScope, RuntimeRealmId,
    RuntimeTaskSpawner, SourceInput, Value,
};
use std::sync::{Arc, Mutex};

const CAP: u64 = 4 * 1024 * 1024;
const LARGE: usize = 8 * 1024 * 1024;
const SETUP_URL: &str = "hosted-completion-defining-source.js";

fn namespace<'s, 'rt>(
    scope: &mut RuntimeNativeScope<'s, 'rt>,
    _capabilities: &CapabilitySet,
    _spawner: Option<RuntimeTaskSpawner>,
) -> Result<RuntimeLocal<'s>, NativeError> {
    let prepare = scope.global_binding("prepareHostedNamespace")?;
    let receiver = scope.undefined();
    scope.call(prepare, receiver, &[])
}

fn commonjs<'s, 'rt>(
    scope: &mut RuntimeNativeScope<'s, 'rt>,
    capabilities: &CapabilitySet,
    spawner: Option<RuntimeTaskSpawner>,
    module: RuntimeLocal<'s>,
    _require: RuntimeLocal<'s>,
) -> Result<RuntimeLocal<'s>, NativeError> {
    let exports = namespace(scope, capabilities, spawner)?;
    scope.set(module, "exports", exports)?;
    Ok(exports)
}

fn failing_namespace<'s, 'rt>(
    scope: &mut RuntimeNativeScope<'s, 'rt>,
    _capabilities: &CapabilitySet,
    _spawner: Option<RuntimeTaskSpawner>,
) -> Result<RuntimeLocal<'s>, NativeError> {
    let mode = scope.global_binding("hostedFailureMode")?;
    match scope.number_value(mode)? as i32 {
        1 => Err(NativeError::SpecError {
            kind: otter_vm::ErrorKind::RangeError,
            message: "native hosted range".into(),
        }),
        4 => Err(NativeError::SpecError {
            kind: otter_vm::ErrorKind::RangeError,
            message: "m".repeat(LARGE),
        }),
        _ => {
            let callback = scope.global_binding("hostedFailureCallback")?;
            let receiver = scope.undefined();
            scope.call(callback, receiver, &[])
        }
    }
}

fn runtime(oom: Arc<Mutex<Option<(u64, u64)>>>) -> Runtime {
    Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .max_heap_bytes(CAP)
        .hosted_module(HostedModule::new("test:static-namespace", namespace))
        .hosted_module(HostedModule::cjs_only("test:static-commonjs", commonjs))
        .hosted_module(HostedModule::new("test:dynamic-namespace", namespace))
        .hosted_module(HostedModule::cjs_only("test:dynamic-commonjs", commonjs))
        .hosted_module(HostedModule::new("test:completion", failing_namespace))
        .extension_installer(RuntimeExtensionInstaller::new(move |realm| {
            realm.install_native_global("hostedChildOffset", 1, |_ctx, args| {
                let child = args
                    .first()
                    .copied()
                    .and_then(Value::as_object)
                    .ok_or(NativeError::InvalidOperand)?;
                Ok(Value::number_i32(child.offset() as i32))
            })?;
            realm.install_native_global("hostedCollect", 0, |ctx, _args| {
                ctx.interp_mut().force_gc().map_err(NativeError::from)?;
                Ok(Value::undefined())
            })?;
            realm.install_native_global("hostedInvalid", 0, |_ctx, _args| {
                Err(NativeError::InvalidOperand)
            })?;
            let oom = oom.clone();
            realm.install_native_global_call(
                "hostedActualAllocation",
                0,
                RuntimeNativeCall::Dynamic(Arc::new(move |ctx, _args, _state| {
                    let result = ctx.scope(|mut scope| {
                        let string = scope.string(&"x".repeat(LARGE))?;
                        Ok(scope.finish(string))
                    });
                    if let Err(error) = &result {
                        let facts = match error {
                            NativeError::OutOfMemory {
                                requested_bytes,
                                heap_limit_bytes,
                                ..
                            } => Some((*requested_bytes, *heap_limit_bytes)),
                            NativeError::ExecutionFailure(failure) => match failure.error {
                                otter_vm::VmError::OutOfMemory {
                                    requested_bytes,
                                    heap_limit_bytes,
                                } => Some((requested_bytes, heap_limit_bytes)),
                                _ => None,
                            },
                            _ => None,
                        };
                        if let Some(facts) = facts {
                            *oom.lock().map_err(|_| NativeError::InvalidOperand)? = Some(facts);
                        }
                    }
                    result
                })),
            )?;
            Ok(())
        }))
        .build()
        .expect("actual configured hosted-module runtime")
}

fn script(
    runtime: &mut Runtime,
    realm: Option<RuntimeRealmId>,
    source: &str,
    name: &str,
) -> Result<crate::ExecutionResult, OtterError> {
    let source = SourceInput::from_javascript(source);
    match realm {
        Some(realm) => runtime.run_script_in_realm(realm, source, name),
        None => runtime.run_script(source, name),
    }
}

fn module(
    runtime: &mut Runtime,
    realm: Option<RuntimeRealmId>,
    source: &str,
    url: &str,
) -> Result<crate::ExecutionResult, OtterError> {
    let source = SourceInput::from_javascript(source);
    match realm {
        Some(realm) => runtime.run_module_source_in_realm(realm, source, url),
        None => runtime.run_module_source(source, url),
    }
}

fn assert_record_owners(runtime: &Runtime) {
    for records in runtime.module_records.realms.values() {
        for (url, record) in records {
            let context = &record.context;
            assert!(
                context.main().id > 0,
                "bootstrap/setup already admitted real code"
            );
            assert!(
                context.function(0).is_none(),
                "compiler-local id is not this chunk's source"
            );
            let init = context
                .module_inits()
                .iter()
                .find(|init| init.url == *url)
                .expect("published record retains its own actual initializer");
            assert_eq!(record.function_id, init.function_id);
            assert!(record.function_id > context.main().id);
            let function = context
                .function(record.function_id)
                .expect("final rebased function id belongs to retained source chunk");
            assert_eq!(function.id, record.function_id);
            assert_eq!(function.module_url, *url);
        }
    }
}

fn records_for(runtime: &Runtime, url: &str) -> usize {
    runtime
        .module_records
        .realms
        .values()
        .filter(|records| records.contains_key(url))
        .count()
}

const PREPARE: &str = r#"
globalThis.hostedInstallCalls = 0;
globalThis.hostedMotion = [];
globalThis.hostedObjects = [];
function prepareHostedNamespace() {
    const object = { marker: ++hostedInstallCalls };
    globalThis.hostedAlias = object;
    hostedObjects.push(object);
    const before = hostedChildOffset(object);
    hostedCollect();
    const after = hostedChildOffset(object);
    if (object !== hostedAlias || object.marker !== hostedInstallCalls)
        throw new Error('hosted callback lost rooted receiver/payload');
    hostedMotion.push([before, after]);
    return object;
}
"#;

#[test]
fn actual_static_dynamic_and_commonjs_instantiation_uses_one_realm_record_owner() {
    let mut runtime = runtime(Arc::new(Mutex::new(None)));
    let additional = runtime.create_realm().expect("actual additional realm");
    for realm in [None, Some(additional)] {
        script(&mut runtime, realm, PREPARE, SETUP_URL).expect("real defining source");
        module(
            &mut runtime,
            realm,
            r#"
            import plain from 'test:static-namespace';
            import cjs from 'test:static-commonjs';
            globalThis.hostedStatic = [plain, cjs];
            if (plain.marker !== 1 || cjs.marker !== 2) throw new Error('static exports');
        "#,
            "file:///hosted-static-entry.mjs",
        )
        .expect("actual static hosted entry");
        assert_record_owners(&runtime);
        // Computed requests enter the actual dynamic-import loader/token rather
        // than graph discovery's literal-string dependency preinstallation.
        script(
            &mut runtime,
            realm,
            r#"
            globalThis.hostedDynamic = null;
            var hostedPlainName = 'test:dynamic-namespace';
            var hostedCjsName = 'test:dynamic-commonjs';
            Promise.all([import(hostedPlainName), import(hostedCjsName)]).then(function(values) {
                globalThis.hostedDynamic = values;
            });
        "#,
            "hosted-dynamic-entry.js",
        )
        .expect("actual dynamic hosted route");
        assert_record_owners(&runtime);
        script(
            &mut runtime,
            realm,
            r#"
            Promise.all([import(hostedPlainName), import(hostedCjsName)]).then(function(values) {
                globalThis.hostedRepeated = values[0] === hostedDynamic[0] &&
                    values[1] === hostedDynamic[1];
            });
        "#,
            "hosted-dynamic-repeat.js",
        )
        .expect("cached dynamic imports");
        let observed = script(
            &mut runtime,
            realm,
            r#"
            [hostedInstallCalls, hostedMotion.length,
             hostedMotion.every(function(pair) { return pair[0] !== pair[1]; }),
             hostedDynamic[0].default === hostedObjects[hostedDynamic[0].marker - 1],
             hostedDynamic[1].default === hostedObjects[hostedDynamic[1].marker - 1],
             hostedDynamic[0].marker + hostedDynamic[1].marker === 7,
             hostedDynamic[0].default !== hostedDynamic[1].default,
             hostedRepeated, hostedStatic[0] === hostedObjects[0],
             hostedStatic[1] === hostedObjects[1]].join(':');
        "#,
            "hosted-canonical-record-report.js",
        )
        .expect("owned alias/motion report");
        assert_eq!(
            observed.completion_string(),
            "4:4:true:true:true:true:true:true:true:true"
        );
        runtime
            .force_gc()
            .expect("real collection after module/callback extents");
        let retained = script(
            &mut runtime,
            realm,
            r#"
            hostedStatic[0].marker + hostedStatic[1].marker +
                hostedDynamic[0].default.marker + hostedDynamic[1].default.marker;
        "#,
            "hosted-retained-records.js",
        )
        .expect("retained real environment roots");
        assert_eq!(retained.completion_string(), "10");
        assert_record_owners(&runtime);
    }
}

const FAILURE_SETUP: &str = r#"
globalThis.hostedFailureMode = 0;
globalThis.hostedOriginal = null;
globalThis.hostedCause = null;
globalThis.hostedFailureMotion = null;
globalThis.hostedCallbackCaught = false;
globalThis.hostedCallbackFinally = false;
function hostedFailureCallback() {
    if (hostedFailureMode === 0) {
        globalThis.hostedCause = new Error('hosted original cause');
        globalThis.hostedOriginal = new Error('hosted original throw', { cause: hostedCause });
        const child = { marker: 73 };
        hostedOriginal.child = child;
        globalThis.hostedFailureAlias = child;
        const before = hostedChildOffset(child);
        hostedCollect();
        const after = hostedChildOffset(child);
        globalThis.hostedFailureMotion = [before, after];
        if (child !== hostedFailureAlias || hostedOriginal.child !== child || child.marker !== 73)
            throw new Error('hosted failure lost alias');
        throw hostedOriginal;
    }
    try {
        if (hostedFailureMode === 2) hostedInvalid();
        if (hostedFailureMode === 3) hostedActualAllocation();
    } catch (error) {
        hostedCallbackCaught = true;
    } finally {
        hostedCallbackFinally = true;
    }
    return {};
}
"#;

#[test]
fn builtin_discovery_absence_does_not_swallow_installer_completions() {
    let mut runtime = runtime(Arc::new(Mutex::new(None)));
    let additional = runtime.create_realm().expect("actual additional realm");
    // `Runtime::create_realm` does not replay the isolate-wide `process`
    // singleton, so `process.getBuiltinModule` is a default-realm surface.
    // Additional-realm installer completions travel through static and
    // dynamic imports in the neighbouring tests.
    assert_eq!(
        script(
            &mut runtime,
            Some(additional),
            "typeof process",
            "additional-realm-process.js"
        )
        .expect("additional realm global probe")
        .completion_string(),
        "undefined"
    );
    {
        let realm = None;
        let setup = format!("{PREPARE}\n{FAILURE_SETUP}");
        script(&mut runtime, realm, &setup, SETUP_URL).expect("actual installer callback source");
        let observed = script(
            &mut runtime,
            realm,
            r#"
            const cacheBefore = Object.getOwnPropertyDescriptor(globalThis, '__otterRequireCache');
            const missing = process.getBuiltinModule('test:unknown-builtin');
            const cacheAfter = Object.getOwnPropertyDescriptor(globalThis, '__otterRequireCache');
            const absent = missing === undefined &&
                (cacheBefore === undefined ? cacheAfter === undefined :
                    cacheAfter !== undefined && cacheBefore.value === cacheAfter.value);

            let original = false;
            try { process.getBuiltinModule('test:completion'); }
            catch (error) {
                original = error === hostedOriginal && error.cause === hostedCause &&
                    error.child === hostedFailureAlias && error.child.marker === 73;
            }
            const moved = hostedFailureMotion[0] !== hostedFailureMotion[1];
            const originalRolledBack = !Object.hasOwn(__otterRequireCache, 'test:completion');
            hostedFailureMode = 1;
            let range = false;
            try { process.getBuiltinModule('test:completion'); }
            catch (error) {
                range = error instanceof RangeError &&
                    Object.getPrototypeOf(error) === RangeError.prototype &&
                    error.message === 'native hosted range';
            }
            [absent, original, moved, originalRolledBack, range,
             !Object.hasOwn(__otterRequireCache, 'test:completion')].join(':');
        "#,
            "builtin-discovery-completion-report.js",
        )
        .expect("absence, actual moving throw identity and native SpecError");
        assert_eq!(
            observed.completion_string(),
            "true:true:true:true:true:true"
        );

        let error = script(
            &mut runtime,
            realm,
            r#"
            hostedFailureMode = 2;
            hostedCallbackCaught = false;
            hostedCallbackFinally = false;
            globalThis.hostedBuiltinOuterCatch = false;
            globalThis.hostedBuiltinOuterFinally = false;
            globalThis.hostedBuiltinAfter = false;
            try { process.getBuiltinModule('test:completion'); }
            catch (error) { hostedBuiltinOuterCatch = true; }
            finally { hostedBuiltinOuterFinally = true; }
            hostedBuiltinAfter = true;
        "#,
            "builtin-structural-terminal.js",
        )
        .expect_err("actual installer structural failure escapes source handlers");
        assert!(
            matches!(error, OtterError::Internal { ref code, .. }
                if code == DiagnosticCode::VmBytecodeInvariant.as_str()),
            "{error:?}"
        );
        assert_eq!(
            script(
                &mut runtime,
                realm,
                r#"
                [hostedCallbackCaught, hostedCallbackFinally,
                 hostedBuiltinOuterCatch, hostedBuiltinOuterFinally, hostedBuiltinAfter,
                 Object.hasOwn(__otterRequireCache, 'test:completion')].join(':');
            "#,
                "builtin-terminal-recovery-report.js",
            )
            .expect("owned post-turn flags and failed-cache rollback")
            .completion_string(),
            "false:false:false:false:false:false"
        );
    }
}

#[test]
fn hosted_failures_keep_original_rejection_fatal_source_and_allocator_domains() {
    let oom = Arc::new(Mutex::new(None));
    let mut runtime = runtime(oom.clone());
    let additional = runtime.create_realm().expect("actual additional realm");
    for (index, realm) in [None, Some(additional)].into_iter().enumerate() {
        let setup = format!("{PREPARE}\n{FAILURE_SETUP}");
        script(&mut runtime, realm, &setup, SETUP_URL).expect("admitted throw source");
        let error = module(
            &mut runtime,
            realm,
            "import partial from 'test:static-namespace'; import hosted from 'test:completion'; globalThis.hostedBodyRan = true;",
            "file:///hosted-throw-entry.mjs",
        )
        .expect_err("actual installer throw");
        // The earlier successful installer survives a later failure in the
        // same linked graph with its final FID/context already published.
        assert_eq!(records_for(&runtime, "test:static-namespace"), index + 1);
        assert_eq!(records_for(&runtime, "test:completion"), 0);
        assert_eq!(records_for(&runtime, "file:///hosted-throw-entry.mjs"), 0);
        assert_record_owners(&runtime);
        let OtterError::Runtime { diagnostic } = error else {
            panic!("original runtime throw");
        };
        assert!(
            diagnostic.message.contains("hosted original throw"),
            "{diagnostic:?}"
        );
        assert_eq!(
            diagnostic
                .cause
                .as_ref()
                .map(|cause| cause.message.as_str()),
            Some("Error: hosted original cause")
        );
        let frame = diagnostic
            .frames
            .iter()
            .find(|frame| frame.function == "hostedFailureCallback" && frame.module == SETUP_URL)
            .expect("exact defining-source callback frame survives native projection");
        let position = frame
            .source_position
            .as_ref()
            .expect("owned exact source position");
        assert_eq!(position.script_name, SETUP_URL);
        assert!(
            position
                .source_line
                .as_ref()
                .contains("throw hostedOriginal")
        );
        assert_eq!(script(&mut runtime, realm,
            "hostedFailureMotion[0] !== hostedFailureMotion[1] && hostedMotion.length === 1 && hostedMotion[0][0] !== hostedMotion[0][1] && hostedObjects[0].marker === 1 && typeof hostedBodyRan === 'undefined';",
            "hosted-static-throw-report.js").unwrap().completion_string(), "true");

        // The failed installer published no environment/cache entry, so a
        // later actual dynamic request reaches it again and preserves identity.
        script(
            &mut runtime,
            realm,
            r#"
            globalThis.hostedRejection = false;
            var hostedFailureName = 'test:completion';
            import(hostedFailureName).catch(function(error) {
                hostedRejection = error === hostedOriginal && error.cause === hostedCause &&
                    error.child === hostedFailureAlias && error.child.marker === 73;
            });
        "#,
            "hosted-original-rejection.js",
        )
        .expect("catchable original dynamic rejection");
        assert_eq!(records_for(&runtime, "test:completion"), 0);
        assert_record_owners(&runtime);
        assert_eq!(
            script(
                &mut runtime,
                realm,
                "hostedRejection && hostedFailureMotion[0] !== hostedFailureMotion[1];",
                "hosted-original-rejection-report.js"
            )
            .unwrap()
            .completion_string(),
            "true"
        );

        script(
            &mut runtime,
            realm,
            r#"
            hostedFailureMode = 1;
            globalThis.hostedRangeRejection = false;
            import(hostedFailureName).catch(function(error) {
                hostedRangeRejection = error instanceof RangeError &&
                    Object.getPrototypeOf(error) === RangeError.prototype &&
                    error.message === 'native hosted range';
            });
        "#,
            "hosted-native-range.js",
        )
        .expect("native class materialized in selected realm");
        assert_eq!(
            script(
                &mut runtime,
                realm,
                "hostedRangeRejection;",
                "hosted-native-range-report.js"
            )
            .unwrap()
            .completion_string(),
            "true"
        );

        for mode in [2, 3, 4] {
            let input = format!(
                r#"
                hostedFailureMode = {mode};
                hostedCallbackCaught = false;
                hostedCallbackFinally = false;
                globalThis.hostedOuterCatch = false;
                globalThis.hostedOuterFinally = false;
                import(hostedFailureName).catch(function() {{ hostedOuterCatch = true; }})
                    .finally(function() {{ hostedOuterFinally = true; }});
            "#
            );
            let before = runtime.interp.gc_heap().gc_cycle_counts();
            let error = script(&mut runtime, realm, &input, "hosted-terminal-dynamic.js")
                .expect_err("completed installation stops the turn");
            match mode {
                2 => assert!(
                    matches!(error, OtterError::Internal { ref code, .. }
                    if code == DiagnosticCode::VmBytecodeInvariant.as_str()),
                    "{error:?}"
                ),
                3 => {
                    let actual = oom
                        .lock()
                        .unwrap()
                        .expect("actual allocator cause recorded outside ABI");
                    assert!(actual.0 > LARGE as u64);
                    assert_eq!(actual.1, CAP);
                    assert!(
                        matches!(error, OtterError::OutOfMemory { requested_bytes, heap_limit_bytes }
                        if (requested_bytes, heap_limit_bytes) == actual),
                        "{error:?}"
                    );
                }
                4 => {
                    assert!(
                        matches!(error, OtterError::OutOfMemory { requested_bytes, heap_limit_bytes }
                        if requested_bytes > LARGE as u64 && heap_limit_bytes == CAP),
                        "{error:?}"
                    );
                    let after = runtime.interp.gc_heap().gc_cycle_counts();
                    assert!(
                        after.0 > before.0 || after.1 > before.1,
                        "actual intrinsic materializer tries collection before cap refusal"
                    );
                }
                _ => unreachable!(),
            }
            assert_eq!(script(&mut runtime, realm,
                "[hostedCallbackCaught, hostedCallbackFinally, hostedOuterCatch, hostedOuterFinally].join(':');",
                "hosted-explicit-terminal-recovery.js").unwrap().completion_string(),
                "false:false:false:false");
            assert_eq!(records_for(&runtime, "test:completion"), 0);
            assert_record_owners(&runtime);
        }
    }
}

#[test]
fn namespace_refusal_preserves_actual_allocator_cause_and_retries_the_same_environment() {
    let mut runtime = runtime(Arc::new(Mutex::new(None)));
    let additional = runtime.create_realm().expect("actual additional realm");
    for realm in [None, Some(additional)] {
        script(&mut runtime, realm, PREPARE, SETUP_URL).expect("actual namespace installer source");
        module(
            &mut runtime,
            realm,
            "import value from 'test:static-namespace'; globalThis.hostedStatic = [value];",
            "file:///hosted-namespace-pressure-entry.mjs",
        )
        .expect("real environment without an eager namespace import");
    }
    runtime
        .force_gc()
        .expect("settle setup garbage before fresh children");
    let realm_ids: Vec<_> = runtime.module_records.realms.keys().copied().collect();
    assert_eq!(realm_ids.len(), 2);
    for (realm, realm_id) in [None, Some(additional)].into_iter().zip(realm_ids) {
        // The script creates the payload's global and pressure slots. Its own
        // garbage (a grown global slab, completion and job records) is then
        // reclaimed: a cap filled with reclaimable bytes is legitimately
        // readmitted by the allocator's emergency full collection.
        script(
            &mut runtime,
            realm,
            r#"
            globalThis.hostedNamespaceFresh = { marker: 91 };
            globalThis.hostedNamespaceAlias = hostedNamespaceFresh;
            hostedStatic[0].pressureChild = hostedNamespaceFresh;
        "#,
            "hosted-namespace-fresh-slots.js",
        )
        .expect("payload slots after setup/previous realm collection");
        runtime
            .force_gc()
            .expect("settle the slot script before the fresh child");
        let fresh_cycles = runtime.interp.gc_heap().gc_cycle_counts();
        runtime
            .interp
            .with_host_realm_id(realm_id, |interp| {
                // A young payload born after the settle replaces the existing
                // slot values, so the cap below is filled by live data only.
                otter_vm::NativeCtx::with_host_context(
                    interp,
                    otter_vm::NativeCallInfo::default_call(),
                    None,
                    |ctx| {
                        ctx.scope(|mut scope| -> Result<(), NativeError> {
                            let child = scope.object()?;
                            let marker = scope.number(91.0);
                            scope.set(child, "marker", marker)?;
                            let global = scope.global_this();
                            scope.set(global, "hostedNamespaceFresh", child)?;
                            scope.set(global, "hostedNamespaceAlias", child)?;
                            let statics = scope.global_binding("hostedStatic")?;
                            let first = scope.get(statics, "0")?;
                            scope.set(first, "pressureChild", child)
                        })
                    },
                )
                .expect("fresh young payload in its existing slots");
                assert_eq!(
                    interp.gc_heap().gc_cycle_counts(),
                    fresh_cycles,
                    "fresh preparation does not collect before the namespace refusal"
                );
                let child_offset = |interp: &mut otter_vm::Interpreter| {
                    otter_vm::NativeCtx::with_host_context(
                        interp,
                        otter_vm::NativeCallInfo::default_call(),
                        None,
                        |ctx| {
                            ctx.scope(|mut scope| {
                                let offset = scope.global_binding("hostedChildOffset")?;
                                let child = scope.global_binding("hostedNamespaceFresh")?;
                                let receiver = scope.undefined();
                                let value = scope.call(offset, receiver, &[child])?;
                                scope.number_value(value)
                            })
                        },
                    )
                    .expect("current child observed through actual native call and handles")
                };
                let before_offset = child_offset(interp);
                let before_cycles = interp.gc_heap().gc_cycle_counts();
                let amount = CAP
                    .checked_sub(interp.gc_heap().tracked_bytes())
                    .expect("setup below real cap");
                let reservation = otter_vm::NativeCtx::with_host_context(
                    interp,
                    otter_vm::NativeCallInfo::default_call(),
                    None,
                    |ctx| ctx.reserve_external(amount),
                )
                .expect("canonical external reservation fills actual effective headroom");
                assert_eq!(interp.gc_heap().tracked_bytes(), CAP);
                assert_eq!(
                    interp.gc_heap().gc_cycle_counts(),
                    before_cycles,
                    "reservation itself does not collect the fresh child"
                );
                let actual = interp
                    .get_or_create_module_namespace("test:static-namespace")
                    .expect_err("cold namespace allocation refuses at the full real cap");
                let native = NativeError::from(actual);
                let (requested, limit) = match &native {
                    NativeError::OutOfMemory {
                        requested_bytes,
                        heap_limit_bytes,
                        ..
                    } => (*requested_bytes, *heap_limit_bytes),
                    _ => panic!("actual allocator retains its existing OOM domain"),
                };
                assert!(requested > 0);
                assert_eq!(limit, CAP);
                let crate::DynLoadError::Fatal(error) =
                    crate::hosted_completion::into_dynamic(interp, native)
                else {
                    panic!("completed namespace allocation must stop before promise rejection");
                };
                assert!(
                    matches!(error, OtterError::OutOfMemory { requested_bytes, heap_limit_bytes }
                if (requested_bytes, heap_limit_bytes) == (requested, limit)),
                    "{error:?}"
                );
                let after_cycles = interp.gc_heap().gc_cycle_counts();
                assert!(after_cycles.0 > before_cycles.0 || after_cycles.1 > before_cycles.1);
                let after_offset = child_offset(interp);
                assert_ne!(
                    before_offset, after_offset,
                    "real refusal collection moved the rooted young child"
                );
                drop(reservation);
                let namespace = interp
                    .get_or_create_module_namespace("test:static-namespace")
                    .expect("released real reservation permits namespace allocation")
                    .expect("the successful installer environment was retained");
                let before_cached = interp.gc_heap().tracked_bytes();
                let cached_cycles = interp.gc_heap().gc_cycle_counts();
                let cached = interp
                    .get_or_create_module_namespace("test:static-namespace")
                    .expect("cached access cannot allocate")
                    .expect("same namespace");
                assert_eq!(namespace, cached);
                assert_eq!(interp.gc_heap().tracked_bytes(), before_cached);
                assert_eq!(interp.gc_heap().gc_cycle_counts(), cached_cycles);
                Ok(())
            })
            .expect("actual registered source realm");
    }
    for realm in [None, Some(additional)] {
        assert_eq!(script(&mut runtime, realm,
            "hostedNamespaceFresh === hostedNamespaceAlias && hostedStatic[0].pressureChild === hostedNamespaceFresh && hostedNamespaceFresh.marker === 91;",
            "hosted-namespace-refusal-alias-report.js").unwrap().completion_string(), "true");
    }
    assert_record_owners(&runtime);
}
