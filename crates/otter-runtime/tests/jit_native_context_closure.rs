//! Native lexical allocation across actual moving collection and inline capture.
//!
//! # Contents
//! - Own current Graph constructor with an accepted ordinary factory splice.
//! - LAB fits, late collecting closure probes and collector-updated lexical roots.
//! - Per-iteration context copies with distinct retained closure identities.
//! - Default/additional source realms and complete interpreter parity.
//!
//! # Invariants
//! - The factory's receiver differs from the physical constructor's receiver;
//!   its `new.target` is undefined while the outer constructor has a target.
//! - Native observers retain scalar offsets/counters only, never GC values.
//! - The only allocation between the factory observations is its new arrow.
//! - Current entry artifacts, complete dispatch traces and unchanged generations
//!   prove execution; no fabricated host LAB or forced collection substitutes.
//! - Warmup and the first measured phase use stress0; the next phase arms
//!   stress1 immediately before the tested native closure allocation.
//!
//! # See also
//! - `otter-jit/src/allocation/lexical_tests.rs` for complete native cell bytes.
//! - `otter-vm/src/runtime_stubs/closure.rs` for the rooted fixed-value Probe.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest,
    JitDebugTarget, JitDebugTier, JitSelection, Runtime, RuntimeExtensionInstaller,
    RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError, RuntimeRealmId, RuntimeValue,
    SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{
        CodeLifetimeState, NativeFrameKind, STUB_COPY_CONTEXT_ALLOC, STUB_CREATE_CONTEXT_ALLOC,
        STUB_JIT_MAKE_CLOSURE, STUB_JIT_MAKE_FN,
    },
};
use serde_json::Value as Json;

const SETUP: &str = r#"
lexicalStress(0);
function makeLexicalChild() { return {marker: 731}; }
function lexicalFactory(child, observe) {
  "use strict";
  const before = () => child;
  observe(child, this, before, 0, before);
  const after = () => [child, this, new.target];
  observe(child, this, after, 1, before);
  const plain = function lexicalPlain() { return 9; };
  return [before, after, plain];
}
function makeLexicalReceiver() { return {make: lexicalFactory}; }
function LexicalOwner(child, observe, receiver) {
  "use strict";
  this.pair = receiver.make(child, observe);
  this.local = () => [child, this, new.target];
}
function buildLexical(Ctor, child, observe, receiver) {
  return new Ctor(child, observe, receiver);
}
function iterationClosures(child) {
  "use strict";
  const result = [];
  for (let i = 0; i < 4; i++) result.push(() => [i, child]);
  return result;
}
const lexicalWarmChild = makeLexicalChild();
const lexicalWarmReceiver = makeLexicalReceiver();
for (let warm = 0; warm < 20000; warm++) {
  const own = buildLexical(LexicalOwner, lexicalWarmChild, lexicalObserve, lexicalWarmReceiver);
  own.pair[0](); own.pair[1](); own.pair[2](); own.local();
  const retained = iterationClosures(lexicalWarmChild);
  retained[0](); retained[1](); retained[2](); retained[3]();
}
"#;
const PROBE: &str = r#"
const lexicalProbeChild = makeLexicalChild();
const lexicalProbeReceiver = makeLexicalReceiver();
const lexicalProbeOwn = buildLexical(LexicalOwner, lexicalProbeChild, lexicalObserve, lexicalProbeReceiver);
const lexicalPrior = lexicalProbeOwn.pair[0]();
const lexicalNext = lexicalProbeOwn.pair[1]();
const lexicalLocal = lexicalProbeOwn.local();
JSON.stringify([lexicalPrior === lexicalProbeChild,
  lexicalNext[0] === lexicalProbeChild, lexicalNext[1] === lexicalProbeReceiver,
  lexicalNext[2] === undefined, lexicalLocal[0] === lexicalProbeChild,
  lexicalLocal[1] === lexicalProbeOwn, lexicalLocal[2] === LexicalOwner,
  lexicalProbeOwn.pair[2]() === 9, lexicalProbeOwn.pair[0] !== lexicalProbeOwn.pair[1]]);
"#;
const COPY_PROBE: &str = r#"
const lexicalCopyChild = makeLexicalChild();
const lexicalCopies = iterationClosures(lexicalCopyChild);
const lexicalCopy0 = lexicalCopies[0](), lexicalCopy1 = lexicalCopies[1]();
const lexicalCopy2 = lexicalCopies[2](), lexicalCopy3 = lexicalCopies[3]();
JSON.stringify([lexicalCopy0[0], lexicalCopy1[0], lexicalCopy2[0], lexicalCopy3[0],
  lexicalCopy0[1] === lexicalCopyChild, lexicalCopy1[1] === lexicalCopyChild,
  lexicalCopy2[1] === lexicalCopyChild, lexicalCopy3[1] === lexicalCopyChild,
  lexicalCopies[0] !== lexicalCopies[1], lexicalCopies[1] !== lexicalCopies[2],
  lexicalCopies[2] !== lexicalCopies[3]]);
"#;

#[derive(Default)]
struct Trace {
    names: BTreeMap<u32, String>,
    recording: bool,
    steps: usize,
    ticks: Vec<u32>,
}
struct Tracer(Arc<Mutex<Trace>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self.0.lock().unwrap();
        trace
            .names
            .entry(event.function_id)
            .or_insert_with(|| event.function_name.to_owned());
        if trace.recording {
            trace.steps += 1;
            if trace.ticks.len() < 256 {
                trace.ticks.push(event.function_id);
            }
        }
    }
}
#[derive(Debug)]
struct Observation {
    phase: i32,
    child: u32,
    receiver: u32,
    context: u32,
    before: u32,
    context_alias: bool,
    this_alias: bool,
    new_target_undefined: bool,
    minor: u64,
    slots: u64,
    updates: u64,
    closures: u64,
    typed_ok: u64,
}

fn installer(
    recording: Arc<AtomicBool>,
    collecting: Arc<AtomicBool>,
    observations: Arc<Mutex<Vec<Result<Observation, String>>>>,
) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        let stress_observations = observations.clone();
        realm.install_native_global_call(
            "lexicalStress",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    match args
                        .first()
                        .and_then(|value| value.as_f64())
                        .filter(|value| {
                            value.is_finite()
                                && *value >= 0.0
                                && *value <= u32::MAX as f64
                                && value.fract() == 0.0
                        }) {
                        Some(stride) => ctx
                            .interp_mut()
                            .gc_heap_mut()
                            .set_gc_stress(stride as u32, true),
                        None => stress_observations.lock().unwrap().push(Err(
                            "stress callback requires an unsigned integer stride".into(),
                        )),
                    }
                    Ok(RuntimeValue::undefined())
                },
            )),
        )?;
        let recording = recording.clone();
        let collecting = collecting.clone();
        let observations = observations.clone();
        realm.install_native_global_call(
            "lexicalObserve",
            5,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]|
                      -> Result<RuntimeValue, RuntimeNativeError> {
                    if recording.load(Ordering::Relaxed) {
                        // Native callbacks cannot unwind through generated frames.
                        // Retain an owned failure and assert it after returning to Rust.
                        let observed = (|| -> Result<Observation, String> {
                            let input = |index: usize, name: &str| {
                                args.get(index)
                                    .copied()
                                    .ok_or_else(|| format!("missing observer {name}"))
                            };
                            let phase = input(3, "phase")?
                                .as_f64()
                                .filter(|value| *value == 0.0 || *value == 1.0)
                                .ok_or_else(|| "observer phase must be zero or one".to_owned())?
                                as i32;
                            if phase == 0 && collecting.load(Ordering::Relaxed) {
                                ctx.interp_mut().gc_heap_mut().set_gc_stress(1, true);
                            }
                            let child = input(0, "child")?;
                            let receiver = input(1, "receiver")?;
                            let observed_closure = input(2, "arrow")?;
                            let earlier_closure = input(4, "earlier arrow")?;
                            let vm = ctx.interp_mut();
                            let closure = observed_closure
                                .as_closure(vm.gc_heap())
                                .ok_or_else(|| "observer requires an actual arrow".to_owned())?;
                            let before =
                                earlier_closure.as_closure(vm.gc_heap()).ok_or_else(|| {
                                    "observer requires an earlier retained arrow".to_owned()
                                })?;
                            let context =
                                closure.context(vm.gc_heap()).as_context().ok_or_else(|| {
                                    "factory arrow has no actual captured context".to_owned()
                                })?;
                            let context_alias =
                                before.context(vm.gc_heap()) == closure.context(vm.gc_heap());
                            let this_alias = closure.bound_this(vm.gc_heap()) == Some(receiver);
                            let new_target_undefined =
                                closure.bound_new_target(vm.gc_heap()).is_none();
                            let typed_ok = vm.jit_runtime_stats().alloc_value_stub_ok;
                            let stats = vm.gc_stats_snapshot();
                            Ok(Observation {
                                phase,
                                child: child
                                    .as_object()
                                    .ok_or_else(|| "observer child is not an object".to_owned())?
                                    .offset(),
                                receiver: receiver
                                    .as_object()
                                    .ok_or_else(|| "observer receiver is not an object".to_owned())?
                                    .offset(),
                                context: context.offset(),
                                before: before.handle.offset(),
                                context_alias,
                                this_alias,
                                new_target_undefined,
                                minor: stats.minor_gc_cycles,
                                slots: stats.minor_root_slots_scanned,
                                updates: stats.minor_slot_updates,
                                closures: stats.by_type
                                    [otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as usize]
                                    .alloc_count_total,
                                typed_ok,
                            })
                        })();
                        observations.lock().unwrap().push(observed);
                    }
                    Ok(RuntimeValue::undefined())
                },
            )),
        )
    })
}
fn run(
    runtime: &mut Runtime,
    realm: Option<RuntimeRealmId>,
    source: &str,
    module: &str,
) -> otter_runtime::ExecutionResult {
    match realm {
        Some(realm) => {
            runtime.run_script_in_realm(realm, SourceInput::from_javascript(source), module)
        }
        None => runtime.run_script(SourceInput::from_javascript(source), module),
    }
    .expect("native lexical proof script")
}
fn current(runtime: &Runtime, fid: u32) -> JitCodeGenerationSnapshot {
    let found: Vec<_> = runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .filter(|generation| {
            generation.function_id == fid
                && generation.tier == NativeFrameKind::Optimizing
                && generation.lifecycle == CodeLifetimeState::Installed
                && generation.linked
        })
        .collect();
    assert_eq!(
        found.len(),
        1,
        "one current own Graph entry fid={fid}: {found:?}"
    );
    found.into_iter().next().unwrap()
}
fn artifact<'a>(
    batch: &'a JitArtifactBatch,
    generation: &JitCodeGenerationSnapshot,
) -> &'a JitArtifactBundle {
    let bundle = batch
        .bundles()
        .iter()
        .find(|bundle| bundle.manifest().code_object_id() == generation.code_object_id)
        .expect("exact current emitted generation");
    assert_eq!(bundle.manifest().function_id(), generation.function_id);
    assert_eq!(bundle.manifest().tier(), JitDebugTier::Optimizing);
    assert_eq!(bundle.manifest().entry(), JitDebugTarget::Entry);
    bundle
}
fn json(bundle: &JitArtifactBundle, name: JitArtifactFileName) -> Json {
    serde_json::from_slice(bundle.file(name).unwrap().contents()).unwrap()
}
fn own_region<'a>(regions: &'a [Json], fid: u32, kind: &str) -> &'a Json {
    regions
        .iter()
        .find(|row| {
            row["kind"] == "instruction"
                && row["functionId"] == fid
                && row["operation"]
                    .as_str()
                    .is_some_and(|text| text.contains(kind))
        })
        .unwrap_or_else(|| panic!("actual source fid={fid} native {kind} region"))
}
fn assert_typed_edge(
    bundle: &JitArtifactBundle,
    region: &Json,
    descriptor: otter_vm::native_abi::RuntimeStubDescriptor,
) {
    assert!(region["startOffset"].as_u64().unwrap() < region["endOffset"].as_u64().unwrap());
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    assert!(
        relocations["relocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["target"]["id"] == descriptor.id
                && row["target"]["signature"] == "allocValue3"
                && region["startOffset"].as_u64().unwrap() <= row["startOffset"].as_u64().unwrap()
                && row["startOffset"].as_u64().unwrap() < region["endOffset"].as_u64().unwrap()),
        "exact typed lexical edge {descriptor:?}"
    );
}
fn assert_native(
    runtime: &mut Runtime,
    trace: &Arc<Mutex<Trace>>,
    result: &otter_runtime::ExecutionResult,
    generations: &[JitCodeGenerationSnapshot],
    factory: u32,
) {
    let trace = trace.lock().unwrap();
    assert!(
        trace.steps > 0,
        "the measured script entry must be observed"
    );
    assert_eq!(
        trace.steps,
        trace.ticks.len(),
        "complete measured interpreter trace"
    );
    for fid in generations
        .iter()
        .map(|g| g.function_id)
        .chain(std::iter::once(factory))
    {
        assert!(
            !trace.ticks.contains(&fid),
            "own/inlined source fid={fid} left native execution: {:?}",
            trace.ticks
        );
    }
    for generation in generations {
        let after = current(runtime, generation.function_id);
        assert_eq!(after.code_object_id, generation.code_object_id);
        assert_eq!(after.generated_deopts, generation.generated_deopts);
        assert_eq!(after.active_count, 0);
    }
    let report = result.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    assert!(
        !report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::Bail { .. }
                | JitDebugEvent::EnteredGenerationDeopt { .. }
                | JitDebugEvent::InlineDeoptFrame { .. }
                | JitDebugEvent::CompilePrepared { .. }
        )),
        "no deopt, retirement or new compilation during the measured native phase: {:?}",
        report.events()
    );
}
fn reset_trace(trace: &Arc<Mutex<Trace>>) {
    let mut trace = trace.lock().unwrap();
    trace.recording = true;
    trace.steps = 0;
    trace.ticks.clear();
}

fn case(extra: bool) {
    let recording = Arc::new(AtomicBool::new(false));
    let collecting = Arc::new(AtomicBool::new(false));
    let observations = Arc::new(Mutex::new(Vec::new()));
    let extension = installer(recording.clone(), collecting.clone(), observations.clone());
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(extension.clone())
        .build()
        .unwrap();
    let realm = extra.then(|| {
        runtime
            .create_realm()
            .expect("additional lexical source realm")
    });
    let trace = Arc::new(Mutex::new(Trace::default()));
    runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    let mut setup = run(&mut runtime, realm, SETUP, "native-lexical-setup.js");
    let names = trace.lock().unwrap().names.clone();
    let fid = |name: &str| {
        *names
            .iter()
            .find(|(_, value)| value.as_str() == name)
            .unwrap_or_else(|| panic!("actual source identity {name}: {names:?}"))
            .0
    };
    let subjects = [
        fid("buildLexical"),
        fid("LexicalOwner"),
        fid("iterationClosures"),
    ];
    let factory = fid("lexicalFactory");
    let mut batch = setup.take_jit_artifacts().unwrap();
    for index in 0..=128 {
        let generations = runtime.jit_code_generation_snapshot();
        if subjects.iter().all(|fid| {
            generations.iter().any(|g| {
                g.function_id == *fid
                    && g.tier == NativeFrameKind::Optimizing
                    && g.lifecycle == CodeLifetimeState::Installed
                    && g.linked
            })
        }) {
            break;
        }
        assert!(index < 128, "bounded own entry admission: {generations:?}");
        let source = "buildLexical(LexicalOwner, lexicalWarmChild, lexicalObserve, lexicalWarmReceiver); iterationClosures(lexicalWarmChild);\n".repeat(16);
        let mut admission = run(
            &mut runtime,
            realm,
            &source,
            &format!("native-lexical-admission-{index}.js"),
        );
        batch = batch.merged(admission.take_jit_artifacts().unwrap());
    }
    assert!(!batch.truncated(), "complete current native artifact bank");
    let generations: Vec<_> = subjects
        .into_iter()
        .map(|fid| current(&runtime, fid))
        .collect();
    let constructor = artifact(&batch, &generations[1]);
    let map = json(constructor, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().unwrap();
    for source in [generations[1].function_id, factory] {
        assert_typed_edge(
            constructor,
            own_region(regions, source, "NativeNewContext"),
            STUB_CREATE_CONTEXT_ALLOC,
        );
        assert_typed_edge(
            constructor,
            own_region(regions, source, "NewClosure"),
            STUB_JIT_MAKE_CLOSURE,
        );
    }
    let closures: Vec<_> = regions
        .iter()
        .filter(|row| {
            row["functionId"] == factory
                && row["operation"]
                    .as_str()
                    .is_some_and(|text| text.contains("NewClosure"))
        })
        .collect();
    assert_eq!(
        closures.len(),
        3,
        "actual inline before/after/plain source nodes"
    );
    assert_typed_edge(constructor, closures[2], STUB_JIT_MAKE_FN);
    let points = json(constructor, JitArtifactFileName::Safepoints);
    for closure in &closures {
        assert!(
            points["records"]
                .as_array()
                .unwrap()
                .iter()
                .any(
                    |point| point["inlineFrames"].as_array().is_some_and(|frames| frames
                        .last()
                        .is_some_and(|frame| frame["functionId"] == factory
                            && frame["bytePc"] == closure["bytePc"]))
                ),
            "actual closure source safepoint owns innermost factory PC"
        );
    }
    let copies = artifact(&batch, &generations[2]);
    let map = json(copies, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().unwrap();
    assert_typed_edge(
        copies,
        own_region(regions, generations[2].function_id, "CopyContext"),
        STUB_COPY_CONTEXT_ALLOC,
    );
    assert_typed_edge(
        copies,
        own_region(regions, generations[2].function_id, "NativeNewContext"),
        STUB_CREATE_CONTEXT_ALLOC,
    );
    assert_typed_edge(
        copies,
        own_region(regions, generations[2].function_id, "NewClosure"),
        STUB_JIT_MAKE_CLOSURE,
    );

    // Complete semantic oracle includes exactly the same global declarations
    // and callback entry shape; observations are disabled for the oracle.
    let oracle_extension = installer(
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(Vec::new())),
    );
    let mut oracle = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .extension_installer(oracle_extension)
        .build()
        .unwrap();
    let oracle_realm = extra.then(|| oracle.create_realm().unwrap());
    run(&mut oracle, oracle_realm, SETUP, "native-lexical-setup.js");
    let expected = run(&mut oracle, oracle_realm, PROBE, "native-lexical-oracle.js")
        .completion_string()
        .to_owned();
    assert_eq!(expected, "[true,true,true,true,true,true,true,true,true]");
    let warm_observations = std::mem::take(&mut *observations.lock().unwrap());
    assert!(
        warm_observations.is_empty(),
        "warm stress callback premises: {warm_observations:?}"
    );
    recording.store(true, Ordering::Relaxed);
    for cold in [false, true] {
        collecting.store(cold, Ordering::Relaxed);
        observations.lock().unwrap().clear();
        // Fresh script-local names avoid redeclaring the previous probe's let/const
        // bindings while preserving all source functions and installed generations.
        let source = format!("{{\n{PROBE}\n}}");
        reset_trace(&trace);
        let before = runtime.execution_stats();
        let result = run(
            &mut runtime,
            realm,
            &source,
            if cold {
                "native-lexical-cold.js"
            } else {
                "native-lexical-fit.js"
            },
        );
        let after = runtime.execution_stats();
        let observed = std::mem::take(&mut *observations.lock().unwrap());
        let observed: Vec<_> = observed
            .into_iter()
            .map(|item| {
                item.unwrap_or_else(|failure| panic!("native observer premise failed: {failure}"))
            })
            .collect();
        assert_eq!(result.completion_string(), expected);
        assert_native(&mut runtime, &trace, &result, &generations[..2], factory);
        assert_eq!(
            after.jit_generated_call_deopts,
            before.jit_generated_call_deopts
        );
        assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
        assert_eq!(after.jit_code_generations, before.jit_code_generations);
        assert_eq!(observed.len(), 2, "exact single factory execution");
        let a = &observed[0];
        let b = &observed[1];
        assert_eq!([a.phase, b.phase], [0, 1]);
        assert!(
            a.context_alias
                && b.context_alias
                && a.this_alias
                && b.this_alias
                && a.new_target_undefined
                && b.new_target_undefined,
            "lexical aliases/source: {observed:?}"
        );
        assert_eq!(
            b.closures,
            a.closures + 1,
            "one real arrow allocation inside the measured region"
        );
        if cold {
            assert_eq!(
                b.typed_ok,
                a.typed_ok + 1,
                "the arrow completes exactly once through its typed allocating stub"
            );
            assert!(
                b.minor > a.minor && b.slots > a.slots && b.updates > a.updates,
                "real collecting roots inside the production closure boundary: {observed:?}"
            );
            assert_ne!(a.child, b.child);
            assert_ne!(a.receiver, b.receiver);
            assert_ne!(a.context, b.context);
            assert_ne!(a.before, b.before, "earlier closure is rewritten too");
        } else {
            assert_eq!(
                b.typed_ok, a.typed_ok,
                "actual current generated LAB fit bypasses the runtime allocator"
            );
            assert_eq!(a.minor, b.minor);
            assert_eq!(
                (a.child, a.receiver, a.context, a.before),
                (b.child, b.receiver, b.context, b.before)
            );
        }
    }
    recording.store(false, Ordering::Relaxed);
    reset_trace(&trace);
    let before = runtime.execution_stats();
    let copies = run(
        &mut runtime,
        realm,
        COPY_PROBE,
        "native-lexical-context-copies.js",
    );
    let after = runtime.execution_stats();
    assert_eq!(
        copies.completion_string(),
        "[0,1,2,3,true,true,true,true,true,true,true]"
    );
    assert_native(&mut runtime, &trace, &copies, &generations[2..], factory);
    assert!(after.gc_minor_cycles > before.gc_minor_cycles);
    assert!(after.gc_minor_slot_updates > before.gc_minor_slot_updates);
    assert_eq!(
        copies.completion_string(),
        run(
            &mut oracle,
            oracle_realm,
            COPY_PROBE,
            "native-lexical-copy-oracle.js"
        )
        .completion_string()
    );
}

#[test]
fn own_native_context_closure_fits_and_collects_with_exact_inline_lexical_bindings() {
    case(false);
}
#[test]
fn own_native_context_closure_keeps_additional_source_realm_and_iteration_bindings() {
    case(true);
}
