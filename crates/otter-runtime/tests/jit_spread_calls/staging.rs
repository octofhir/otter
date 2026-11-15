//! Native spread staging across representation, collection and abrupt completion.
//!
//! # Contents
//! - Ordinary-policy admission of exact current spread wrapper generations.
//! - Int32, Float64 and tagged actuals at the inline-buffer boundary and beyond.
//! - Collecting native calls, constructor preparation and exceptions without replay.
//!
//! # Invariants
//! - The probe cannot interpret either spread wrapper or replace its generation.
//! - Array argument identity and numeric edge values survive every collection.
//! - Captured code proves the exact admitted body owns spread staging.
//!
//! # See also
//! - `JitCtx::stage_spread_arguments` snapshots the dense carrier.
//! - VM trampoline staging tests prove raw holes and moving staged-root rewriting.

use otter_runtime::{
    JitArtifactBatch, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitDebugTarget,
    JitSelection, Runtime, RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx,
    RuntimeNativeError, RuntimeValue, SourceInput, ValueWindow,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::JitCodeGenerationSnapshot;
use otter_vm::native_abi::{CodeLifetimeState, NativeFrameKind};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const MODULE: &str = "jit-spread-staging-setup.js";
const SETUP: &str = r#"let spreadEffects = 0;
let spreadBodyEffects = 0;
let spreadGetEffects = 0;
let spreadThrow = false;
let spreadCollect = false;
function stagedCall(fn, args) { spreadEffects++; return fn(...args); }
function stagedNew(Ctor, args) { spreadEffects++; return new Ctor(...args); }
function stagedTarget() {
  spreadBodyEffects++;
  if (spreadCollect) stagingCollect();
  if (spreadThrow) throw new Error("staged-once");
  return Array.prototype.slice.call(arguments);
}
function stagedConstructor() {
  spreadBodyEffects++;
  if (spreadCollect) stagingCollect();
  if (spreadThrow) throw new Error("staged-once");
  this.args = Array.prototype.slice.call(arguments);
}
const stagedProxy = new Proxy(stagedConstructor, {
  get(target, key, receiver) {
    if (key === "prototype") {
      spreadGetEffects++;
      if (spreadCollect) stagingCollect();
    }
    return Reflect.get(target, key, receiver);
  }
});
function verifyStaged(actual, expected) {
  if (actual.length !== expected.length) throw new Error("staged arity");
  for (let i = 0; i < expected.length; i++) {
    if (!Object.is(actual[i], expected[i])) throw new Error("staged argument " + i);
  }
}
let spreadWarmCursor = 0;
function stagingWarmBatch() {
  for (let warm = spreadWarmCursor; warm < spreadWarmCursor + 16; warm++) {
    const args = [warm & 31, 1, 2, 3, 4, 5, 6, 7, 8];
    stagedCall(stagedTarget, args);
    stagedCall(stagingNativeEcho, args);
    stagedCall(stagingNativeThrow, args);
    stagedNew(stagedConstructor, args);
    stagedNew(stagedProxy, args);
  }
  spreadWarmCursor += 16;
}
"#;
const WARM_BATCH: &str = "stagingWarmBatch();";
const PROBE: &str = r#"spreadCollect = true;
const widths = [0, 1, 8, 9, 256];
let checked = 0;
const effectStart = spreadEffects;
const bodyStart = spreadBodyEffects;
const getStart = spreadGetEffects;
for (const width of widths) {
  for (let kind = 0; kind < 3; kind++) {
    const args = [];
    for (let i = 0; i < width; i++) {
      args.push(kind === 0 ? i - 7 : kind === 1 ? (i === 0 ? -0 : i + 0.25) : { marker: i + 11 });
    }
    verifyStaged(stagedCall(stagingNativeEcho, args), args);
    verifyStaged(stagedCall(stagedTarget, args), args);
    verifyStaged(stagedNew(stagedConstructor, args).args, args);
    verifyStaged(stagedNew(stagedProxy, args).args, args);
    spreadThrow = true;
    for (let call = 0; call < 4; call++) {
      let caught = false;
      try {
        if (call === 0) stagedCall(stagedTarget, args);
        else if (call === 1) stagedCall(stagingNativeThrow, args);
        else stagedNew(call === 2 ? stagedConstructor : stagedProxy, args);
      } catch (error) {
        caught = error.message === "staged-once";
      }
      if (!caught) throw new Error("staged throwing completion");
    }
    spreadThrow = false;
    checked++;
  }
}
const observed = [checked, spreadEffects - effectStart, spreadBodyEffects - bodyStart, spreadGetEffects - getStart];
if (JSON.stringify(observed) !== "[15,120,90,30]") throw new Error("staged replay " + JSON.stringify(observed));
JSON.stringify(observed);
"#;

#[derive(Default)]
struct Trace {
    names: BTreeMap<String, u32>,
    recording: bool,
    steps: usize,
    entered: BTreeSet<u32>,
}
struct Tracer(Arc<Mutex<Trace>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self.0.lock().unwrap();
        trace
            .names
            .entry(event.function_name.into())
            .or_insert(event.function_id);
        if trace.recording {
            trace.steps += 1;
            trace.entered.insert(event.function_id);
        }
    }
}

fn current(
    runtime: &Runtime,
    fid: u32,
    tier: NativeFrameKind,
) -> Option<JitCodeGenerationSnapshot> {
    runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .find(|generation| {
            generation.function_id == fid
                && generation.tier == tier
                && generation.lifecycle == CodeLifetimeState::Installed
                && generation.linked
                && generation.current_entry
        })
}

struct NativeObservation {
    sources_before: String,
    sources_after: String,
    generations_before: Vec<JitCodeGenerationSnapshot>,
    generations_after: Vec<JitCodeGenerationSnapshot>,
}

fn collect(
    ctx: &mut RuntimeNativeCtx<'_>,
    observations: &Mutex<Vec<NativeObservation>>,
) -> Result<(), RuntimeNativeError> {
    let sources_before = ctx
        .execution_context()
        .map(|context| ctx.capture_call_sites_json(context, 0, 64))
        .ok_or_else(|| RuntimeNativeError::Error {
            message: "staging observer has no suspended source context".into(),
        })?;
    let generations_before = ctx.interp_mut().jit_code_generation_snapshot();
    ctx.interp_mut()
        .force_gc()
        .map_err(|error| RuntimeNativeError::Error {
            message: error.to_string(),
        })?;
    let generations_after = ctx.interp_mut().jit_code_generation_snapshot();
    let sources_after = ctx
        .execution_context()
        .map(|context| ctx.capture_call_sites_json(context, 0, 64))
        .ok_or_else(|| RuntimeNativeError::Error {
            message: "staging observer lost its suspended source context".into(),
        })?;
    observations.lock().unwrap().push(NativeObservation {
        sources_before,
        sources_after,
        generations_before,
        generations_after,
    });
    Ok(())
}

fn assert_native_observations(
    observations: &[NativeObservation],
    admitted: &[JitCodeGenerationSnapshot],
) {
    assert_eq!(
        observations.len(),
        150,
        "one observation per collecting hook"
    );
    let mut counts = [0; 2];
    for observation in observations {
        let before: serde_json::Value = serde_json::from_str(&observation.sources_before).unwrap();
        let after: serde_json::Value = serde_json::from_str(&observation.sources_after).unwrap();
        assert_eq!(before, after, "suspended source frames survive full GC");
        let frames = before.as_array().unwrap();
        assert!(frames.len() < 64, "complete bounded source capture");
        let wrappers: Vec<_> = frames
            .iter()
            .filter(|frame| {
                frame["functionName"] == "stagedCall" || frame["functionName"] == "stagedNew"
            })
            .collect();
        assert_eq!(wrappers.len(), 1, "one own spread wrapper activation");
        let frame = wrappers[0];
        assert_eq!(frame["scriptName"], MODULE);
        let index = usize::from(frame["functionName"] == "stagedNew");
        let expected_line = if index == 0 {
            "function stagedCall(fn, args) { spreadEffects++; return fn(...args); }"
        } else {
            "function stagedNew(Ctor, args) { spreadEffects++; return new Ctor(...args); }"
        };
        assert_eq!(frame["sourceLine"].as_str().unwrap().trim(), expected_line);
        let expected = &admitted[index];
        for generations in [
            &observation.generations_before,
            &observation.generations_after,
        ] {
            let active = generations
                .iter()
                .find(|generation| generation.code_object_id == expected.code_object_id)
                .expect("the exact wrapper generation remains retained");
            assert_eq!(active.function_id, expected.function_id);
            assert_eq!(active.tier, expected.tier);
            assert_eq!(active.lifecycle, CodeLifetimeState::Installed);
            assert!(
                active.current_entry && active.linked && active.active_count > 0,
                "the exact own native wrapper is active across collection: {active:?}"
            );
        }
        counts[index] += 1;
    }
    // Each case collects in four Call paths and four Construct paths; the
    // two Proxy constructs additionally collect from their prototype getter.
    assert_eq!(counts, [60, 90]);
}

fn assert_staging_artifact(artifacts: &JitArtifactBatch, generation: &JitCodeGenerationSnapshot) {
    assert!(!artifacts.truncated());
    let bundle = artifacts
        .bundles()
        .iter()
        .find(|bundle| bundle.manifest().code_object_id() == generation.code_object_id)
        .expect("exact current native body was captured");
    assert_eq!(bundle.manifest().function_id(), generation.function_id);
    assert_eq!(bundle.manifest().module(), MODULE);
    assert_eq!(bundle.manifest().entry(), JitDebugTarget::Entry);
    let code_map: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::CodeMap)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert_eq!(
        code_map["callEntryOffset"].as_u64(),
        generation.call_entry_offset.map(u64::from)
    );
    assert!(generation.call_entry_offset.is_some());
    let relocations: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::Relocations)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert!(
        relocations["relocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|relocation| {
                relocation["target"]["kind"] == "runtimeStub"
                    && relocation["target"]["name"] == "jit_stage_spread"
            }),
        "the exact current wrapper owns spread argument staging"
    );
    let bytecode = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::Bytecode)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert!(bytecode.contains("CallSpread") || bytecode.contains("NewSpread"));
}

fn admitted_wrappers(
    runtime: &Runtime,
    trace: &Mutex<Trace>,
    tier: NativeFrameKind,
) -> Option<Vec<JitCodeGenerationSnapshot>> {
    let trace = trace.lock().unwrap();
    ["stagedCall", "stagedNew"]
        .into_iter()
        .map(|name| current(runtime, *trace.names.get(name)?, tier))
        .collect()
}

fn acknowledge_pending(
    runtime: &mut Runtime,
    mut artifacts: JitArtifactBatch,
    batch: usize,
) -> JitArtifactBatch {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut poll = 0;
    while runtime.jit_code_residency().pending_compilations != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "normal background staging compilation must acknowledge: {:?}",
            runtime.jit_code_residency()
        );
        // An ordinary fresh script entry polls finished compiler work without
        // funding another wrapper generation while its current job is pending.
        let mut result = runtime
            .run_script(
                SourceInput::from_javascript("0;"),
                &format!("staging-ack-{batch}-{poll}.js"),
            )
            .unwrap();
        artifacts = artifacts.merged(result.take_jit_artifacts().unwrap());
        assert!(!artifacts.truncated());
        poll += 1;
        std::thread::yield_now();
    }
    artifacts
}

fn admit_stable_wrappers(
    runtime: &mut Runtime,
    trace: &Mutex<Trace>,
    mut artifacts: JitArtifactBatch,
    selection: JitSelection,
    tier: NativeFrameKind,
    diagnostics: bool,
) -> (Vec<JitCodeGenerationSnapshot>, JitArtifactBatch) {
    // This is a safety bound, not a target warm count. Each complete batch
    // preserves all five original call families and varies the numeric input.
    // The persistent warmer preserves normal source-work history across
    // ordinary turns. Stop only after both exact desired-tier generations
    // survive another complete batch, including the full 32-value warm cycle,
    // with no pending compilation; no background work is forced.
    const MAX_BATCHES: usize = 512;
    for batch in 0..MAX_BATCHES {
        artifacts = acknowledge_pending(runtime, artifacts, batch * 2);
        let before = admitted_wrappers(runtime, trace, tier);
        let mut result = runtime
            .run_script(
                SourceInput::from_javascript(WARM_BATCH),
                &format!("staging-warm-{batch}.js"),
            )
            .unwrap();
        artifacts = artifacts.merged(result.take_jit_artifacts().unwrap());
        assert!(!artifacts.truncated());
        artifacts = acknowledge_pending(runtime, artifacts, batch * 2 + 1);
        let after = admitted_wrappers(runtime, trace, tier);
        if diagnostics {
            eprintln!(
                "spread-staging {selection:?} warm-batch={batch} completed-iterations={} current={:?}",
                (batch + 1) * 16,
                after.as_ref().map(|generations| generations
                    .iter()
                    .map(|generation| generation.code_object_id)
                    .collect::<Vec<_>>())
            );
        }
        if let (Some(before), Some(after)) = (before, after)
            && (batch + 1) * 16 >= 32
            && before
                .iter()
                .zip(&after)
                .all(|(old, new)| old.code_object_id == new.code_object_id)
        {
            assert_eq!(runtime.jit_code_residency().pending_compilations, 0);
            return (after, artifacts);
        }
    }
    panic!(
        "all-family ordinary warm batches did not retain exact native staging generations: {:?}",
        runtime.jit_code_generation_snapshot()
    );
}

#[test]
fn wide_spread_actuals_survive_native_collection_and_throw_once() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let diagnostics = std::env::var_os("OTTER_SPREAD_STAGING_DIAGNOSTICS").is_some();
        let started = std::time::Instant::now();
        let phase = |name: &str| {
            if diagnostics {
                eprintln!(
                    "spread-staging {selection:?} {name} elapsed={:?}",
                    started.elapsed()
                );
            }
        };
        phase("build-start");
        let native_calls = Arc::new(AtomicUsize::new(0));
        let collecting = Arc::new(AtomicBool::new(false));
        let observations = Arc::new(Mutex::new(Vec::new()));
        let installer = RuntimeExtensionInstaller::new({
            let native_calls = native_calls.clone();
            let collecting = collecting.clone();
            let observations = observations.clone();
            move |realm| {
                let collect_observations = observations.clone();
                realm.install_native_global_call(
                    "stagingCollect",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              _args: ValueWindow<'_>,
                              _state: ValueWindow<'_>| {
                            collect(ctx, &collect_observations)?;
                            Ok(RuntimeValue::undefined())
                        },
                    )),
                )?;
                let echo_calls = native_calls.clone();
                let echo_collecting = collecting.clone();
                let echo_observations = observations.clone();
                realm.install_native_global_call(
                    "stagingNativeEcho",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              args: ValueWindow<'_>,
                              _state: ValueWindow<'_>| {
                            echo_calls.fetch_add(1, Ordering::Relaxed);
                            if echo_collecting.load(Ordering::Relaxed) {
                                collect(ctx, &echo_observations)?;
                            }
                            ctx.scope(|mut scope| {
                                let output = scope.array(args.len())?;
                                for index in 0..args.len() {
                                    let actual = scope.argument(args, index);
                                    scope.set_index(output, index, actual)?;
                                }
                                Ok(scope.finish(output))
                            })
                        },
                    )),
                )?;
                let throw_calls = native_calls.clone();
                let throw_collecting = collecting.clone();
                let throw_observations = observations.clone();
                realm.install_native_global_call(
                    "stagingNativeThrow",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              _args: ValueWindow<'_>,
                              _state: ValueWindow<'_>| {
                            throw_calls.fetch_add(1, Ordering::Relaxed);
                            if !throw_collecting.load(Ordering::Relaxed) {
                                return Ok(RuntimeValue::undefined());
                            }
                            collect(ctx, &throw_observations)?;
                            Err(RuntimeNativeError::Error {
                                message: "staged-once".into(),
                            })
                        },
                    )),
                )
            }
        });
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .extension_installer(installer)
            .build()
            .unwrap();
        let trace = Arc::new(Mutex::new(Trace::default()));
        runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
        phase("build-done");
        phase("setup-start");
        let mut setup = runtime
            .run_script(SourceInput::from_javascript(SETUP), MODULE)
            .unwrap();
        phase("setup-done");
        let artifacts = setup.take_jit_artifacts().unwrap();
        let tier = if selection == JitSelection::Template {
            NativeFrameKind::Baseline
        } else {
            NativeFrameKind::Optimizing
        };
        phase("admission-start");
        let (before, artifacts) = admit_stable_wrappers(
            &mut runtime,
            &trace,
            artifacts,
            selection,
            tier,
            diagnostics,
        );
        for generation in &before {
            assert_staging_artifact(&artifacts, generation);
        }
        phase("admission-done");
        assert!(observations.lock().unwrap().is_empty());
        let stats_before = runtime.execution_stats();
        native_calls.store(0, Ordering::Relaxed);
        collecting.store(true, Ordering::Relaxed);
        trace.lock().unwrap().recording = true;
        phase("probe-start");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(PROBE),
                "jit-spread-staging-probe.js",
            )
            .unwrap();
        phase("probe-done");
        trace.lock().unwrap().recording = false;
        assert_eq!(result.completion_string(), "[15,120,90,30]");
        assert_eq!(native_calls.load(Ordering::Relaxed), 30);
        let trace = trace.lock().unwrap();
        assert!(
            trace.steps > 0,
            "the interpreter tracer observed the probe boundary"
        );
        for generation in &before {
            assert!(
                !trace.entered.contains(&generation.function_id),
                "the admitted spread wrapper stays native"
            );
            let after = current(&runtime, generation.function_id, tier).unwrap();
            assert_eq!(after.code_object_id, generation.code_object_id);
            assert_eq!(after.generated_deopts, generation.generated_deopts);
            assert_eq!(after.active_count, 0);
            assert!(
                !result
                    .jit_debug_report()
                    .unwrap()
                    .events()
                    .iter()
                    .any(|event| {
                        match event {
                            JitDebugEvent::Bail { function_id, .. } => {
                                *function_id == generation.function_id
                            }
                            JitDebugEvent::EnteredGenerationDeopt {
                                callee_function_id, ..
                            } => *callee_function_id == generation.function_id,
                            _ => false,
                        }
                    })
            );
        }
        assert_native_observations(&observations.lock().unwrap(), &before);
        let report = result.jit_debug_report().unwrap();
        assert!(!report.truncated());
        assert_eq!(report.dropped_events(), 0);
        assert!(runtime.execution_stats().gc_cycles > stats_before.gc_cycles);
        phase("assertions-done");
    }
}
