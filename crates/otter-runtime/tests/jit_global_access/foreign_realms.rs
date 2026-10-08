//! Source-realm binding completion through actual foreign native activations.
//!
//! # Contents
//! - Safe same-isolate callable exchange through the existing persistent table.
//! - Actual foreign Template/Graph entries compiled in the caller's realm.
//! - Own source/return records, moving actuals and strict existence snapshots.
//!
//! # Invariants
//! Only an owned PersistentRootId leaves the native scope. Source work earns
//! admission without policy changes. The foreign body may not bake the caller's
//! globals or splice into it; its committed reads and writes retain the defining
//! realm. Callback records own text/scalars and are asserted after the ABI returns.
//!
//! # See also
//! - otter_vm::global_ops owns the source-aware committed semantics.
//! - support/return_sites joins source roots to actual machine call returns.

use super::*;
use otter_vm::PersistentRootId;

#[path = "../support/return_sites.rs"]
mod return_sites;

const FOREIGN: &str = "foreign-binding-source.js";
const DEFINE: &str = r#"
let realmLexical = 17;
let realmExpectedGlobal = globalThis;
globalThis.realmObjectValue = 23;
globalThis.realmExclusive = 31;
function realmForeign(child, observe, writing) {
  'use strict';
  observe(child);
  const before = realmLexical;
  const object = realmObjectValue;
  const ownGlobal = globalThis === realmExpectedGlobal;
  if (writing) {
    realmLexical = before + 1;
    realmObjectValue = object + 1;
    realmExclusive = realmExclusive + 1;
  }
  return [before, object, ownGlobal, child.marker,
    realmLexical, realmObjectValue, realmExclusive];
}
realmSave([realmForeign, realmObserve]);
"#;
const WARM: &str = r#"
let realmLexical = 101;
globalThis.realmObjectValue = 201;
const foreignBundle = realmLoad();
const warmChild = {marker:731};
for (let warm = 0; warm < 8000; warm++) foreignBundle[0](warmChild, foreignBundle[1], 0);
8000;
"#;
const PROBE: &str = r#"
const child = {marker:731};
const answer = foreignBundle[0](child, foreignBundle[1], 1);
JSON.stringify([answer, realmLexical, realmObjectValue, typeof realmExclusive]);
"#;
const EXPECTED: &str = "[[17,23,true,731,18,24,32],101,201,\"undefined\"]";

struct Observation {
    before: String,
    after: String,
    before_generations: Vec<JitCodeGenerationSnapshot>,
    after_generations: Vec<JitCodeGenerationSnapshot>,
    motion: Vec<moving_children::ChildMotion>,
}
#[derive(Default)]
struct Exchange {
    retained: Mutex<Option<PersistentRootId>>,
    recording: AtomicBool,
    observations: Mutex<Vec<Result<Observation, String>>>,
}
fn installer(exchange: Arc<Exchange>) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        moving_children::install(realm)?;
        let saved = exchange.clone();
        realm.install_native_global_call(
            "realmSave",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _: &[RuntimeValue]| {
                    ctx.scope(|mut scope| {
                        let callable = scope.argument(args, 0);
                        let mut retained = saved
                            .retained
                            .lock()
                            .map_err(|_| RuntimeNativeError::InvalidOperand)?;
                        if retained.is_some() {
                            return Err(RuntimeNativeError::InvalidOperand);
                        }
                        *retained = Some(scope.persistent_root_insert(callable));
                        Ok(RuntimeValue::undefined())
                    })
                },
            )),
        )?;
        let loaded = exchange.clone();
        realm.install_native_global_call(
            "realmLoad",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>, _: &[RuntimeValue], _: &[RuntimeValue]| {
                    let id = loaded
                        .retained
                        .lock()
                        .map_err(|_| RuntimeNativeError::InvalidOperand)?
                        .take()
                        .ok_or(RuntimeNativeError::InvalidOperand)?;
                    ctx.scope(|mut scope| {
                        let callable = scope
                            .take_persistent_root(id)
                            .ok_or(RuntimeNativeError::InvalidOperand)?;
                        Ok(scope.finish(callable))
                    })
                },
            )),
        )?;
        let observed = exchange.clone();
        realm.install_native_global_call(
            "realmObserve",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _: &[RuntimeValue]| {
                    if observed.recording.load(Ordering::Relaxed) {
                        let before = ctx
                            .execution_context()
                            .map(|source| ctx.capture_call_sites_json(source, 0, 32));
                        let before_generations = ctx.interp_mut().jit_code_generation_snapshot();
                        let moved = moving_children::observe_and_collect(ctx, args);
                        let after_generations = ctx.interp_mut().jit_code_generation_snapshot();
                        let after = ctx
                            .execution_context()
                            .map(|source| ctx.capture_call_sites_json(source, 0, 32));
                        let record = match (before, after, moved) {
                            (Some(before), Some(after), Ok(motion)) => Ok(Observation {
                                before,
                                after,
                                before_generations,
                                after_generations,
                                motion,
                            }),
                            (_, _, Err(error)) => {
                                Err(format!("foreign moving argument: {error:?}"))
                            }
                            _ => Err("foreign caller has no source context".to_owned()),
                        };
                        observed
                            .observations
                            .lock()
                            .map_err(|_| RuntimeNativeError::InvalidOperand)?
                            .push(record);
                    }
                    Ok(RuntimeValue::undefined())
                },
            )),
        )
    })
}

fn native_proof(
    runtime: &Runtime,
    warm: &otter_runtime::ExecutionResult,
    tier: NativeFrameKind,
) -> Proof {
    let artifacts = warm.jit_artifacts().unwrap();
    assert!(!artifacts.truncated());
    let generations = runtime.jit_code_generation_snapshot();
    let proofs: Vec<_> = artifacts
        .bundles()
        .iter()
        .filter(|bundle| {
            bundle.manifest().module() == FOREIGN
                && bundle.manifest().function_name() == "realmForeign"
        })
        .filter_map(|bundle| {
            generations
                .iter()
                .find(|generation| {
                    generation.function_id == bundle.manifest().function_id()
                        && generation.code_object_id == bundle.manifest().code_object_id()
                        && generation.tier == tier
                        && generation.current_entry
                        && generation.linked
                        && generation.lifecycle == CodeLifetimeState::Installed
                })
                .map(|generation| Proof {
                    bundle: bundle.clone(),
                    generation: generation.clone(),
                })
        })
        .collect();
    assert_eq!(
        proofs.len(),
        1,
        "one actually installed foreign callable: {generations:?}"
    );
    proofs.into_iter().next().unwrap()
}

#[test]
fn foreign_callable_binding_misses_keep_defining_realm_and_moving_actuals() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let exchange = Arc::new(Exchange::default());
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .extension_installer(installer(exchange.clone()))
            .build()
            .unwrap();
        let trace = Arc::new(Mutex::new(TraceLog {
            recording: true,
            functions: Vec::new(),
        }));
        runtime.set_tracer(Some(Box::new(Trace(trace.clone()))));
        let realm = runtime.create_realm().unwrap();
        runtime
            .run_script_in_realm(realm, SourceInput::from_javascript(DEFINE), FOREIGN)
            .unwrap();
        assert!(exchange.retained.lock().unwrap().is_some());
        // The only callable handoff is the existing persistent owner. Warming
        // happens in the default realm, not in the callee's creation realm.
        let warm = runtime
            .run_script(
                SourceInput::from_javascript(WARM),
                "foreign-binding-warm.js",
            )
            .unwrap();
        assert_eq!(warm.completion_string(), "8000");
        assert!(exchange.retained.lock().unwrap().is_none());
        assert!(
            trace
                .lock()
                .unwrap()
                .functions
                .iter()
                .any(|name| name == "realmForeign"),
            "the pre-admission interpreter tracer observes the actual source"
        );
        let proof = match selection {
            JitSelection::InterpreterOnly => None,
            JitSelection::Template => {
                Some(native_proof(&runtime, &warm, NativeFrameKind::Baseline))
            }
            JitSelection::ProductionTiered => {
                Some(native_proof(&runtime, &warm, NativeFrameKind::Optimizing))
            }
        };
        if let Some(proof) = &proof {
            let bundle = &proof.bundle;
            let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
            let offset = proof.generation.call_entry_offset.unwrap();
            assert!((offset as usize) < code.len());
            assert_eq!(
                json(bundle, JitArtifactFileName::CodeMap)["callEntryOffset"].as_u64(),
                Some(u64::from(offset))
            );
            let relocations = json(bundle, JitArtifactFileName::Relocations);
            assert!(
                !relocations["relocations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["target"]["kind"] == "globalLexicalCell"),
                "a foreign compile never bakes the default realm's same-spelled lexical"
            );
            return_sites::assert_sites(bundle);
            let events = warm.jit_debug_report().unwrap();
            assert!(!events.truncated());
            assert_eq!(events.dropped_events(), 0);
            assert!(!events.events().iter().any(|event| matches!(event, JitDebugEvent::InlineLowered { callee_function_id, .. } if *callee_function_id == proof.generation.function_id)), "the foreign body has its own activation");
            let map = json(bundle, JitArtifactFileName::CodeMap);
            let regions = map["regions"].as_array().unwrap();
            let bytecode = binding_bytecode(bundle);
            for opcode in [
                "LoadGlobalOrThrow",
                "LoadGlobalThis",
                "GlobalBindingExists",
                "StoreGlobalChecked",
            ] {
                assert!(
                    bytecode.values().any(|actual| actual == opcode),
                    "actual own foreign binding operation {opcode}"
                );
            }
            let global_this = bytecode
                .into_iter()
                .find(|(_, op)| op == "LoadGlobalThis")
                .unwrap()
                .0;
            assert!(
                relocations["relocations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| {
                        entry["target"]["kind"] == "runtimeStub"
                            && entry["target"]["name"] == "jit_binding_value"
                            && regions.iter().any(|region| {
                                region["bytePc"].as_u64() == Some(u64::from(global_this))
                                    && region["startOffset"].as_u64()
                                        <= entry["startOffset"].as_u64()
                                    && entry["endOffset"].as_u64() <= region["endOffset"].as_u64()
                            })
                    }),
                "own globalThis operation has a real committed source miss"
            );
        }
        trace.lock().unwrap().functions.clear();
        exchange.recording.store(true, Ordering::Relaxed);
        let stats = runtime.execution_stats();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(PROBE),
                "foreign-binding-probe.js",
            )
            .unwrap();
        exchange.recording.store(false, Ordering::Relaxed);
        assert_eq!(result.completion_string(), EXPECTED, "{selection:?}");
        assert!(runtime.execution_stats().gc_cycles > stats.gc_cycles);
        let records = exchange.observations.lock().unwrap();
        assert_eq!(
            records.len(),
            1,
            "one actual observer, without source replay"
        );
        let record = records[0].as_ref().unwrap();
        assert_eq!(record.motion.len(), 1);
        assert_ne!(record.motion[0].before, record.motion[0].after);
        assert_eq!(record.motion[0].marker_before, 731.0);
        assert_eq!(record.motion[0].marker_after, 731.0);
        for source in [&record.before, &record.after] {
            let frames: serde_json::Value = serde_json::from_str(source).unwrap();
            let frames = frames.as_array().unwrap();
            assert_eq!(frames[0]["functionName"], "realmForeign");
            assert!(frames[0]["scriptName"].as_str().unwrap().ends_with(FOREIGN));
            assert_eq!(frames[0]["sourceLine"], "  observe(child);");
        }
        let trace = trace.lock().unwrap();
        assert!(
            !trace.functions.is_empty(),
            "actual probe dispatch is observed"
        );
        if let Some(proof) = &proof {
            assert!(
                !trace.functions.iter().any(|name| name == "realmForeign"),
                "the own foreign body stays native: {:?}",
                trace.functions
            );
            for generations in [&record.before_generations, &record.after_generations] {
                let own: Vec<_> = generations
                    .iter()
                    .filter(|generation| {
                        generation.function_id == proof.generation.function_id
                            && generation.current_entry
                    })
                    .collect();
                assert_eq!(own.len(), 1);
                assert_eq!(own[0].code_object_id, proof.generation.code_object_id);
                assert!(
                    own[0].active_count > 0,
                    "own native lease remains published through real GC"
                );
                assert_eq!(own[0].call_entry_offset, proof.generation.call_entry_offset);
                assert_eq!(own[0].generated_deopts, proof.generation.generated_deopts);
            }
            let after: Vec<_> = runtime
                .jit_code_generation_snapshot()
                .into_iter()
                .filter(|generation| {
                    generation.function_id == proof.generation.function_id
                        && generation.current_entry
                })
                .collect();
            assert_eq!(after.len(), 1);
            assert_eq!(after[0].code_object_id, proof.generation.code_object_id);
            assert_eq!(after[0].active_count, 0);
            assert_eq!(after[0].generated_deopts, proof.generation.generated_deopts);
            assert_eq!(
                runtime.execution_stats().jit_code_generations,
                stats.jit_code_generations
            );
            let report = result.jit_debug_report().unwrap();
            assert!(!report.truncated());
            assert_eq!(report.dropped_events(), 0);
            assert!(!report.events().iter().any(|event| matches!(
                event,
                JitDebugEvent::CompilePrepared { .. }
                    | JitDebugEvent::Bail { .. }
                    | JitDebugEvent::EnteredGenerationDeopt { .. }
                    | JitDebugEvent::InlineDeoptFrame { .. }
            )));
        } else {
            assert!(trace.functions.iter().any(|name| name == "realmForeign"));
            assert!(record.before_generations.is_empty());
            assert!(record.after_generations.is_empty());
        }
    }
}
