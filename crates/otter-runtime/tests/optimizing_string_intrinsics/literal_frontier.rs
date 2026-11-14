//! Retained generated literal loads across cache growth and collection.
//!
//! # Contents
//! - Normal background admission of exact String and BigInt reader generations.
//! - Own-source relocations for warm and eagerly prepared cold literal sites.
//! - Native reuse after moving collection, promotion and full collection.
//!
//! # Invariants
//! - No subject interpreter dispatch, generation replacement or deopt satisfies
//!   a native probe; each read observes its exact optimizing generation active.
//! - Pinned source churn grows the literal cache without an eval reclamation.
//! - Runtime GC counters prove a moving collection occurred, while the VM
//!   literal-root fixtures separately prove individual cell relocation.
//!
//! # See also
//! - `otter_vm::interp::tests::literal_roots` checks exact root-slot membership.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use otter_bytecode::Op;
use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitSelection, Runtime,
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError,
    RuntimeValue, SourceInput, ValueWindow,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{CodeLifetimeState, NativeFrameKind},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    sync::{Arc, Mutex},
};

const READERS: [&str; 2] = ["frontierStringLiteral", "frontierBigIntLiteral"];
const DEFINITIONS: &str = r#"
let literalGrowthChecksum = 0;
globalThis.__literalFrontierCarrier = undefined;
function frontierStringLiteral(cold) {
  if (arguments === cold) throw 17;
  const result = cold ? "literal frontier cold Ω" : "literal frontier warm";
  const child = frontierFreshChild();
  frontierObserve(child);
  return result;
}
function frontierBigIntLiteral(cold) {
  if (arguments === cold) throw 19;
  const result = cold ? 123456789012345678901234567890n : 9007199254740993n;
  const child = frontierFreshChild();
  frontierObserve(child);
  return result;
}
for (let warm = 0; warm < 1024; warm++) {
  frontierStringLiteral(false);
  frontierBigIntLiteral(false);
}
"ready";
"#;
const PROBE: &str = r#"
JSON.stringify([
  frontierStringLiteral(false),
  frontierStringLiteral(true),
  String(frontierBigIntLiteral(false)),
  String(frontierBigIntLiteral(true))
]);
"#;
const EXPECTED: &str = r#"["literal frontier warm","literal frontier cold Ω","9007199254740993","123456789012345678901234567890"]"#;

#[path = "../support/moving_children.rs"]
mod moving_children;

#[derive(Debug)]
struct Observation {
    generations_before: Vec<JitCodeGenerationSnapshot>,
    generations_after: Vec<JitCodeGenerationSnapshot>,
    sources_before: String,
    sources_after: String,
    children: Vec<moving_children::ChildMotion>,
}

#[derive(Default)]
struct Trace {
    warm_literals: BTreeMap<u32, BTreeSet<u32>>,
    recording: bool,
    ids: Vec<u32>,
    observations: Vec<Observation>,
}

fn observer_installer(trace: Arc<Mutex<Trace>>) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        moving_children::install(realm)?;
        let fresh_trace = trace.clone();
        realm.install_native_global_call(
            "frontierFreshChild",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      _args: ValueWindow<'_>,
                      _state: ValueWindow<'_>| {
                    if !fresh_trace.lock().unwrap().recording {
                        return Ok(RuntimeValue::undefined());
                    }
                    ctx.scope(|mut scope| {
                        let layout = scope.object_layout(&["marker"])?;
                        let marker = scope.number(37.0);
                        let child = scope.object_with_layout(layout, &[marker])?;
                        Ok(scope.finish(child))
                    })
                },
            )),
        )?;
        let trace = trace.clone();
        realm.install_native_global_call(
            "frontierObserve",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: ValueWindow<'_>,
                      _state: ValueWindow<'_>| {
                    if !trace.lock().unwrap().recording {
                        return Ok(RuntimeValue::undefined());
                    }
                    let sources_before = ctx
                        .execution_context()
                        .map(|context| ctx.capture_call_sites_json(context, 0, 64))
                        .ok_or_else(|| RuntimeNativeError::Error {
                            message: "literal observer has no suspended source context".to_owned(),
                        })?;
                    let generations_before = ctx.interp_mut().jit_code_generation_snapshot();
                    let children = moving_children::observe_and_collect(ctx, args)?;
                    let generations_after = ctx.interp_mut().jit_code_generation_snapshot();
                    let sources_after = ctx
                        .execution_context()
                        .map(|context| ctx.capture_call_sites_json(context, 0, 64))
                        .ok_or_else(|| RuntimeNativeError::Error {
                            message: "literal observer lost its suspended source context"
                                .to_owned(),
                        })?;
                    trace.lock().unwrap().observations.push(Observation {
                        generations_before,
                        generations_after,
                        sources_before,
                        sources_after,
                        children,
                    });
                    Ok(RuntimeValue::undefined())
                },
            )),
        )
    })
}

struct Tracer(Arc<Mutex<Trace>>);

impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self.0.lock().unwrap();
        if trace.recording {
            assert!(
                trace.ids.len() < 16_384,
                "complete bounded native probe trace"
            );
            trace.ids.push(event.function_id);
        } else if READERS.contains(&event.function_name)
            && matches!(event.op, Op::LoadString | Op::LoadBigInt)
        {
            trace
                .warm_literals
                .entry(event.function_id)
                .or_default()
                .insert(event.byte_pc);
        }
    }
}

fn current(
    runtime: &Runtime,
    artifacts: &BTreeMap<u64, JitArtifactBundle>,
    name: &str,
) -> Option<JitCodeGenerationSnapshot> {
    runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .find(|generation| {
            generation.lifecycle == CodeLifetimeState::Installed
                && generation.linked
                && generation.current_entry
                && generation.tier == NativeFrameKind::Optimizing
                && artifacts
                    .get(&generation.code_object_id)
                    .is_some_and(|bundle| bundle.manifest().function_name() == name)
        })
}

fn literal_sites(bundle: &JitArtifactBundle, function_id: u32) -> BTreeSet<u32> {
    let relocations: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::Relocations)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let sites = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|relocation| relocation["target"]["kind"] == "literalCell")
        .map(|relocation| {
            let target = &relocation["target"];
            assert_eq!(target["functionId"].as_u64(), Some(u64::from(function_id)));
            u32::try_from(target["bytePc"].as_u64().unwrap()).unwrap()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        sites.len(),
        2,
        "both warm and cold literal sites are native: {relocations}"
    );
    let map: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::CodeMap)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let emitted = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["operation"]
                    .as_str()
                    .is_some_and(|operation| operation.contains(" LoadLiteral("))
        })
        .map(|region| {
            assert_eq!(region["functionId"].as_u64(), Some(u64::from(function_id)));
            u32::try_from(region["bytePc"].as_u64().unwrap()).unwrap()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        emitted, sites,
        "relocations belong to actual emitted literal operations"
    );
    sites
}

fn assert_retained(
    runtime: &Runtime,
    artifacts: &BTreeMap<u64, JitArtifactBundle>,
    originals: &[JitCodeGenerationSnapshot],
) {
    for (name, original) in READERS.iter().zip(originals) {
        let retained =
            current(runtime, artifacts, name).expect("reader remains current and installed");
        assert_eq!(
            retained.code_object_id, original.code_object_id,
            "same {name} generation"
        );
        assert_eq!(retained.generated_deopts, original.generated_deopts);
        assert_eq!(retained.active_count, 0);
    }
}

fn probe(
    runtime: &mut Runtime,
    artifacts: &BTreeMap<u64, JitArtifactBundle>,
    originals: &[JitCodeGenerationSnapshot],
    trace: &Arc<Mutex<Trace>>,
    phase: &str,
) {
    assert_eq!(runtime.jit_code_residency().pending_compilations, 0);
    assert_retained(runtime, artifacts, originals);
    {
        let mut trace = trace.lock().unwrap();
        trace.ids.clear();
        trace.observations.clear();
        trace.recording = true;
    }
    let before = runtime.execution_stats();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(PROBE),
            &format!("literal-frontier-{phase}.js"),
        )
        .unwrap();
    trace.lock().unwrap().recording = false;
    assert_eq!(result.completion_string(), EXPECTED);
    let after = runtime.execution_stats();
    let report = result.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    assert!(
        !report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::CompilePrepared { .. }
                | JitDebugEvent::Bail { .. }
                | JitDebugEvent::EnteredGenerationDeopt { .. }
                | JitDebugEvent::InlineDeoptFrame { .. }
        )),
        "native literal probe has no compilation, fallback or replay: {:?}",
        report.events()
    );
    let trace = trace.lock().unwrap();
    assert!(!trace.ids.is_empty(), "the script entry is actually traced");
    for original in originals {
        assert!(
            !trace.ids.contains(&original.function_id),
            "literal reader must not dispatch interpreted instructions"
        );
    }
    assert_retained(runtime, artifacts, originals);
    // Ordinary Graph calls use the private call ABI and deliberately bypass
    // materialized-entry counters. Observe each exact live native generation
    // after its LoadLiteral instead of treating that counter as a call count.
    assert_eq!(
        trace.observations.len(),
        4,
        "one observation per literal read; no replay"
    );
    for (index, observation) in trace.observations.iter().enumerate() {
        let expected = &originals[index / 2];
        for generations in [
            &observation.generations_before,
            &observation.generations_after,
        ] {
            let active = generations
                .iter()
                .find(|generation| generation.code_object_id == expected.code_object_id)
                .expect("the observed reader generation remains retained");
            assert_eq!(active.function_id, expected.function_id);
            assert_eq!(active.tier, NativeFrameKind::Optimizing);
            assert_eq!(active.lifecycle, CodeLifetimeState::Installed);
            assert!(
                active.current_entry && active.linked && active.active_count > 0,
                "{phase}: the exact native reader is active while collecting: {active:?}"
            );
        }
        assert_eq!(observation.children.len(), 1);
        let child = &observation.children[0];
        assert_ne!(
            child.before, child.after,
            "the exact young child moves inside the reader"
        );
        assert_eq!(child.marker_before, 37.0);
        assert_eq!(child.marker_after, 37.0);
        let sources_before: serde_json::Value =
            serde_json::from_str(&observation.sources_before).unwrap();
        let sources_after: serde_json::Value =
            serde_json::from_str(&observation.sources_after).unwrap();
        assert_eq!(
            sources_before, sources_after,
            "suspended native sources survive collection"
        );
        let matching: Vec<_> = sources_before
            .as_array()
            .unwrap()
            .iter()
            .filter(|frame| frame["functionName"] == READERS[index / 2])
            .collect();
        assert_eq!(matching.len(), 1, "one own reader source activation");
        assert_eq!(matching[0]["scriptName"], "literal-frontier-admission-0.js");
        assert_eq!(
            matching[0]["sourceLine"].as_str().unwrap().trim(),
            "frontierObserve(child);"
        );
    }
    assert!(after.gc_cycles >= before.gc_cycles + 4);
    assert!(after.gc_minor_slot_updates > before.gc_minor_slot_updates);
    println!(
        "literal-native phase={phase} codeObjectIds={:?} activeNativeObservations={} fullCollections={} minorSlotUpdates={}",
        originals
            .iter()
            .map(|generation| generation.code_object_id)
            .collect::<Vec<_>>(),
        trace.observations.len(),
        after.gc_cycles - before.gc_cycles,
        after.gc_minor_slot_updates - before.gc_minor_slot_updates,
    );
}

#[test]
fn exact_native_literal_cells_survive_growth_promotion_and_full_gc() {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(observer_installer(trace.clone()))
        .build()
        .unwrap();
    runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    let mut artifacts = BTreeMap::new();
    for batch in 0..=128 {
        let source = if batch == 0 {
            DEFINITIONS.to_owned()
        } else {
            format!(
                "{}\n\"ready\";",
                "frontierStringLiteral(false); frontierBigIntLiteral(false);\n".repeat(32)
            )
        };
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source),
                &format!("literal-frontier-admission-{batch}.js"),
            )
            .unwrap();
        assert_eq!(result.completion_string(), "ready");
        let captured = result.jit_artifacts().unwrap();
        assert!(!captured.truncated());
        for bundle in captured.bundles() {
            artifacts.insert(bundle.manifest().code_object_id(), bundle.clone());
        }
        if runtime.jit_code_residency().pending_compilations == 0
            && READERS
                .iter()
                .all(|name| current(&runtime, &artifacts, name).is_some())
        {
            break;
        }
        assert!(batch < 128, "ordinary background Graph admission");
    }
    let originals = READERS
        .iter()
        .map(|name| current(&runtime, &artifacts, name).unwrap())
        .collect::<Vec<_>>();
    for generation in &originals {
        let sites = literal_sites(
            &artifacts[&generation.code_object_id],
            generation.function_id,
        );
        let trace = trace.lock().unwrap();
        let executed = &trace.warm_literals[&generation.function_id];
        assert_eq!(executed.len(), 1, "warmup executes only one literal branch");
        assert!(executed.is_subset(&sites));
        assert_eq!(
            sites.difference(executed).count(),
            1,
            "the other literal was eagerly prewarmed for native code"
        );
    }
    probe(&mut runtime, &artifacts, &originals, &trace, "cold-native");

    // These global slots were installed before admission, so growth changes
    // neither the global shape nor the declarative binding-name epoch.
    let mut growth = String::from("literalGrowthChecksum = 0;\n");
    let mut expected_growth = 0usize;
    for index in 0..256 {
        let spelling = format!("pinned-literal-frontier-growth-{index}");
        writeln!(
            growth,
            "literalGrowthChecksum += \"{spelling}\".length + Number({}n);",
            1000 + index
        )
        .unwrap();
        expected_growth += spelling.len() + 1000 + index;
    }
    growth.push_str("globalThis.__literalFrontierCarrier = {left: {marker: 37}, right: [3, 5, 8]};\nliteralGrowthChecksum;");
    let before_gc = runtime.execution_stats();
    let grown = runtime
        .run_script(
            SourceInput::from_javascript(growth),
            "literal-frontier-growth.js",
        )
        .unwrap();
    assert_eq!(grown.completion_string(), expected_growth.to_string());
    runtime
        .force_gc()
        .expect("move live values after literal cache growth");
    let moved = runtime.execution_stats();
    assert!(moved.gc_minor_cycles > before_gc.gc_minor_cycles);
    assert!(
        moved.gc_minor_slot_updates > before_gc.gc_minor_slot_updates,
        "the collection actually updates young root slots"
    );
    probe(
        &mut runtime,
        &artifacts,
        &originals,
        &trace,
        "after-moving-gc",
    );
    for round in 0..2 {
        runtime
            .force_gc()
            .expect("promote and retain cached literals");
        probe(
            &mut runtime,
            &artifacts,
            &originals,
            &trace,
            &format!("full-gc-{round}"),
        );
    }
    let carrier = runtime.run_script(
        SourceInput::from_javascript("JSON.stringify([__literalFrontierCarrier.left.marker, __literalFrontierCarrier.right]);"),
        "literal-frontier-carrier.js",
    ).unwrap();
    assert_eq!(carrier.completion_string(), "[37,[3,5,8]]");
    assert_retained(&runtime, &artifacts, &originals);
}
