//! Exact completed error propagation through installed selected Native entries.
//!
//! # Contents
//! - Authored Native OOM retains one catchable intrinsic RangeError projection.
//! - Actual error-building OOM escapes direct/nested calls, Apply/Bind,
//!   collecting Proxy traps and native constructor prototype lookup.
//! - Moving arguments, suspended source and current native generation ownership.
//!
//! # Invariants
//! - Normal source work admits the same existing wrapper before each probe.
//! - Own emitted Native and Generic CALL/return records remain required.
//! - Callback observations contain owned data; assertions run after native return.
//! - Failed completed Error materialization escapes without a second JS build.
//!
//! # See also
//! - `super::assert_native_edge` joins actual emitted code to canonical roots.
//! - `otter_vm::NativeError::ExecutionFailure` transports final owned failures.

use super::*;
use otter_runtime::{OtterError, RuntimeExtensionContext};

const CAP: u64 = 4 * 1024 * 1024;

pub(super) fn install(
    realm: &mut RuntimeExtensionContext<'_>,
    records: Arc<Mutex<Vec<Result<Observation, String>>>>,
) -> Result<(), OtterError> {
    realm.install_native_global("nativeKindAuthoredOOM", 0, |_ctx, _args| {
        Err(RuntimeNativeError::OutOfMemory {
            name: "authored native OOM",
            requested_bytes: 719,
            heap_limit_bytes: 4096,
        })
    })?;
    realm.install_native_global_call(
        "nativeKindCompletion",
        1,
        RuntimeNativeCall::Dynamic(Arc::new(move |ctx, args, _state| {
            let before = ctx
                .execution_context()
                .map(|context| ctx.capture_call_sites_json(context, 0, usize::MAX));
            let generations_before = ctx.interp_mut().jit_code_generation_snapshot();
            let children = moving_children::observe_and_collect(ctx, args);
            let generations_after = ctx.interp_mut().jit_code_generation_snapshot();
            let after = ctx
                .execution_context()
                .map(|context| ctx.capture_call_sites_json(context, 0, usize::MAX));
            let observed = match (before, after, children) {
                (Some(before), Some(after), Ok(children)) => Ok(Observation {
                    before,
                    after,
                    generations_before,
                    generations_after,
                    children,
                }),
                (_, _, Err(error)) => Err(format!("completion movement: {error:?}")),
                _ => Err("completion native source context missing".into()),
            };
            records
                .lock()
                .map_err(|_| RuntimeNativeError::InvalidOperand)?
                .push(observed);
            Err(RuntimeNativeError::SyntaxError {
                name: "completed native source",
                reason: "m".repeat(8 * 1024 * 1024),
            })
        })),
    )?;
    realm.install_native_global_call(
        "nativeKindNestedCompletion",
        1,
        RuntimeNativeCall::Dynamic(Arc::new(|ctx, args, _state| {
            ctx.scope(|mut scope| {
                let target = scope
                    .global("nativeKindCompletion")
                    .ok_or(RuntimeNativeError::InvalidOperand)?;
                let child = scope.argument(args, 0);
                let receiver = scope.undefined();
                let result = scope.call(target, receiver, &[child])?;
                Ok(scope.finish(result))
            })
        })),
    )
}

pub(super) fn execute(
    fixture: &mut Fixture,
    source: &str,
    module: &str,
) -> Result<otter_runtime::ExecutionResult, OtterError> {
    let source = SourceInput::from_javascript(source);
    match fixture.realm {
        Some(realm) => fixture.runtime.run_script_in_realm(realm, source, module),
        None => fixture.runtime.run_script(source, module),
    }
}

pub(super) fn assert_observed_source(
    json: &str,
    generations: &[JitCodeGenerationSnapshot],
    own: &JitCodeGenerationSnapshot,
    name: &str,
    source_line: &str,
    module: &str,
) {
    let frames: Json = serde_json::from_str(json).unwrap();
    let found: Vec<_> = frames
        .as_array()
        .unwrap()
        .iter()
        .filter(|frame| frame["functionName"] == name)
        .collect();
    assert_eq!(found.len(), 1, "exact suspended caller {frames}");
    assert!(found[0]["scriptName"].as_str().unwrap().ends_with(module));
    assert_eq!(found[0]["sourceLine"], source_line);
    let retained: Vec<_> = generations
        .iter()
        .filter(|generation| {
            generation.code_object_id == own.code_object_id
                && generation.function_id == own.function_id
                && generation.tier == own.tier
        })
        .collect();
    assert_eq!(retained.len(), 1);
    let retained = retained[0];
    assert_eq!(retained.lifecycle, CodeLifetimeState::Installed);
    assert!(retained.linked && retained.current_entry);
    assert_eq!(retained.active_count, 1);
    assert_eq!(retained.generated_deopts, own.generated_deopts);
}

fn assert_generic_edge(bundle: &JitArtifactBundle) {
    let links = json(bundle, JitArtifactFileName::Relocations);
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let sites = return_sites::assert_sites(bundle);
    let links: Vec<_> = links["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|link| {
            link["target"]["kind"] == "runtimeStub"
                && link["target"]["id"].as_u64() == Some(u64::from(abi::STUB_JIT_CALL_GENERIC.id))
        })
        .collect();
    assert_eq!(links.len(), 1, "actual committed Generic miss edge");
    let end = links[0]["endOffset"].as_u64().unwrap() as usize;
    #[cfg(target_arch = "aarch64")]
    let call: &[u8] = &[0x00, 0x02, 0x3f, 0xd6];
    #[cfg(target_arch = "x86_64")]
    let call: &[u8] = &[0x41, 0xff, 0xd3];
    assert_eq!(code.get(end..end + call.len()), Some(call));
    assert_eq!(
        sites["returnSites"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|site| site["nativeReturnOffset"].as_u64() == Some((end + call.len()) as u64))
            .count(),
        1
    );
}

#[test]
fn real_completed_oom_escapes_generic_selected_native_and_nested_source_realms() {
    for extra in [false, true] {
        for selection in [
            JitSelection::InterpreterOnly,
            JitSelection::Template,
            JitSelection::ProductionTiered,
        ] {
            let mut fixture = Fixture::new_capped(selection, extra, Some(CAP));
            if selection != JitSelection::InterpreterOnly {
                fixture.admit();
            }
            // The caller's parametric call site already trained Native kind.
            // Changing visible RangeError cannot redirect its pinned prototype.
            let authored = fixture.run(
                "const completionOriginalRange = RangeError.prototype; let completionCtorCalls=0; globalThis.RangeError=function changedRange(){completionCtorCalls++;throw 997;}; let completionAuthored; try{knownNativeOne(nativeKindAuthoredOOM,undefined);}catch(error){completionAuthored=[error.name,error.message.includes('719'),error.message.includes('4096'),Object.getPrototypeOf(error)===completionOriginalRange];} JSON.stringify([completionAuthored,completionCtorCalls]);",
                "authored-native-oom.js",
            );
            assert_eq!(
                authored.completion_string(),
                "[[\"RangeError\",true,true,true],0]"
            );
            let routes = [
                ("knownNativeOne", "nativeKindCompletion", false),
                ("knownNativeOne", "nativeKindNestedCompletion", false),
                (
                    "knownNativeOne",
                    "new Proxy(nativeKindCompletion,{})",
                    false,
                ),
                (
                    "knownNativeOne",
                    "function completionApply(value){return Function.prototype.apply.call(nativeKindCompletion,undefined,[value]);}",
                    false,
                ),
                (
                    "knownNativeOne",
                    "function completionBind(value){const target=new Proxy(nativeKindCompletion,{get(target,key){if(key==='length')return nativeKindCompletion(value);return Reflect.get(target,key);}});return Function.prototype.bind.call(target,undefined);}",
                    false,
                ),
                (
                    "knownNativeOne",
                    "new Proxy(nativeKindCompletion,{get apply(){return nativeKindCompletion(CHILD);}})",
                    false,
                ),
                (
                    "knownNativeConstruct",
                    "new Proxy(Array,{get(target,key){if(key==='prototype')return nativeKindCompletion(CHILD);return Reflect.get(target,key);}})",
                    true,
                ),
            ];
            for (index, (name, target, construct)) in routes.into_iter().enumerate() {
                let target = target.replace("CHILD", &format!("completionChild{index}"));
                let own = fixture.current(name);
                assert_eq!(
                    own.is_some(),
                    selection != JitSelection::InterpreterOnly,
                    "native selections retain an exact current subject"
                );
                if let Some(own) = &own {
                    assert!(own.current_entry && own.call_entry_offset.is_some());
                    let bundle = &fixture.artifacts[&own.code_object_id];
                    assert_native_edge(bundle);
                    assert_generic_edge(bundle);
                }
                fixture.records.lock().unwrap().clear();
                fixture.trace.lock().unwrap().recording = true;
                let before = fixture.runtime.execution_stats();
                let source = format!(
                    "const completionChild{index}=nativeKindRenew(719); try{{{name}({target},completionChild{index});}}catch(error){{throw 997;}} 41;"
                );
                let result = execute(&mut fixture, &source, &format!("completed-oom-{index}.js"));
                fixture.trace.lock().unwrap().recording = false;
                let error = result.expect_err("actual Error build failure cannot enter a JS catch");
                assert!(
                    matches!(error, OtterError::OutOfMemory {requested_bytes, heap_limit_bytes}
                    if requested_bytes > CAP && heap_limit_bytes == CAP),
                    "{error:?}"
                );
                assert!(fixture.runtime.execution_stats().gc_cycles > before.gc_cycles);
                let trace = fixture.trace.lock().unwrap();
                assert!(trace.total > 0);
                assert_eq!(trace.total, trace.counts.values().sum::<usize>());
                if let Some(own) = &own {
                    assert_eq!(trace.counts.get(&own.function_id).copied().unwrap_or(0), 0);
                    let current = fixture.current(name).unwrap();
                    assert_eq!(current.code_object_id, own.code_object_id);
                    assert_eq!(current.generated_deopts, own.generated_deopts);
                    assert_eq!(current.active_count, 0);
                }
                drop(trace);
                let records = fixture.records.lock().unwrap();
                assert_eq!(records.len(), 1, "one reached native callback, no replay");
                let record = records[0]
                    .as_ref()
                    .unwrap_or_else(|error| panic!("{error}"));
                let [child] = record.children.as_slice() else {
                    panic!("one actual rooted child");
                };
                assert_eq!(child.marker_before, 719.0);
                assert_eq!(child.marker_after, 719.0);
                assert_ne!(child.before, child.after, "exact argument evacuation");
                if let Some(own) = &own {
                    let line = if construct {
                        "  const result = new ctor(value); return [result.length, result[0] === value];"
                    } else {
                        "function knownNativeOne(fn, value) { return fn(value); }"
                    };
                    assert_observed_source(
                        &record.before,
                        &record.generations_before,
                        own,
                        name,
                        line,
                        MODULE,
                    );
                    assert_observed_source(
                        &record.after,
                        &record.generations_after,
                        own,
                        name,
                        line,
                        MODULE,
                    );
                }
            }
        }
    }
}
