//! Selected NativeFunction entries through both installed native tiers.
//!
//! # Contents
//! - Static-to-dynamic target replacement at an unchanged kind plan.
//! - Zero/odd actuals, moving aliases, throws and live constructability.
//! - Exact current calls, return records, source and suspended code leases.
//!
//! # Invariants
//! - Normal source work admits each own function; no policy is lowered.
//! - Warm and fresh children use the same scoped final-shape factory.
//! - Throws propagate out of the native subject; catches live in the outer probe.
//! - Callbacks retain owned records and never assert or unwind.
//! - Complete dispatch counts omit the subjects during each isolated probe.
//! - Only explicitly requested inspection reads static code byte sizes.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitDebugTarget,
    JitSelection, Runtime, RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx,
    RuntimeNativeError, RuntimeRealmId, RuntimeValue, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{self as abi, CodeLifetimeState, NativeFrameKind},
};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
#[path = "jit_known_native_calls/completion.rs"]
mod completion;
#[path = "jit_known_native_calls/mixed_get.rs"]
mod mixed_get;
#[allow(dead_code)]
#[path = "support/moving_children.rs"]
mod moving_children;
#[allow(dead_code)]
#[path = "support/return_sites.rs"]
mod return_sites;
const MODULE: &str = "known-native-setup.js";
const DEFINITIONS: &str = r#"
const nativeWarmA = nativeKindRenew(41), nativeWarmB = nativeKindRenew(42);
function knownNativeZero(fn) { return fn(); }
function knownNativeOne(fn, value) { return fn(value); }
function knownNativeOdd(fn, a, b, c) {
  const result = fn(a, b, c);
  return [result, a.marker, b.marker, c.marker, a === c];
}
function knownNativeConstruct(ctor, value) {
  const result = new ctor(value); return [result.length, result[0] === value];
}
function knownNativeThrow(fn) { const result = fn(); return result; }
function knownNativeReplacement(a, b, c) { return 2000 + arguments.length; }
"#;
const WARM: &str = "knownNativeZero(nativeKindA); knownNativeOne(nativeKindA,nativeWarmA); knownNativeOdd(nativeKindA,nativeWarmA,nativeWarmB,nativeWarmA); knownNativeConstruct(Array,nativeWarmA); knownNativeThrow(nativeKindA);";
const NAMES: [&str; 5] = [
    "knownNativeZero",
    "knownNativeOne",
    "knownNativeOdd",
    "knownNativeConstruct",
    "knownNativeThrow",
];
#[derive(Default)]
struct Trace {
    recording: bool,
    total: usize,
    counts: BTreeMap<u32, usize>,
}
struct Tracer(Arc<Mutex<Trace>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if trace.recording {
            trace.total += 1;
            *trace.counts.entry(event.function_id).or_default() += 1;
        }
    }
}
struct Observation {
    before: String,
    after: String,
    generations_before: Vec<JitCodeGenerationSnapshot>,
    generations_after: Vec<JitCodeGenerationSnapshot>,
    children: Vec<moving_children::ChildMotion>,
}
fn installer(
    collect: Arc<AtomicBool>,
    records: Arc<Mutex<Vec<Result<Observation, String>>>>,
    completion_failures: bool,
) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        moving_children::install(realm)?;
        if completion_failures {
            completion::install(realm, records.clone())?;
        }
        // Deliberately absent from the audited leaf registry.
        realm.install_native_global("nativeKindA", 0, |_ctx, args| {
            Ok(RuntimeValue::number_i32(args.len() as i32))
        })?;
        let collect = collect.clone();
        let records = records.clone();
        realm.install_native_global_call(
            "nativeKindB",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    let count = args.len();
                    if collect.load(Ordering::Relaxed) && count == 3 {
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
                            (_, _, Err(error)) => Err(format!("actual movement: {error:?}")),
                            _ => Err("missing native source context before/after GC".into()),
                        };
                        records
                            .lock()
                            .map_err(|_| RuntimeNativeError::Error {
                                message: "native recorder poisoned".into(),
                            })?
                            .push(observed);
                    }
                    Ok(RuntimeValue::number_i32(1000 + count as i32))
                },
            )),
        )?;
        realm.install_native_global_call(
            "nativeKindThrow",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                |_ctx: &mut RuntimeNativeCtx<'_>,
                 _args: &[RuntimeValue],
                 _state: &[RuntimeValue]| {
                    Err(RuntimeNativeError::TypeError {
                        name: "nativeKindThrow",
                        reason: "native-kind-throw".into(),
                    })
                },
            )),
        )?;
        realm.install_native_global_call(
            "nativeKindRenew",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _state: &[RuntimeValue]| {
                    ctx.scope(|mut scope| {
                        let marker = scope.argument(args, 0);
                        let layout = scope.object_layout(&["marker"])?;
                        let child = scope.object_with_layout(layout, &[marker])?;
                        Ok(scope.finish(child))
                    })
                },
            )),
        )
    })
}
struct Fixture {
    runtime: Runtime,
    realm: Option<RuntimeRealmId>,
    selection: JitSelection,
    sequence: u32,
    artifacts: BTreeMap<u64, JitArtifactBundle>,
    events: Vec<JitDebugEvent>,
    trace: Arc<Mutex<Trace>>,
    collect: Arc<AtomicBool>,
    records: Arc<Mutex<Vec<Result<Observation, String>>>>,
}
impl Fixture {
    fn new(selection: JitSelection, extra: bool) -> Self {
        Self::new_capped(selection, extra, None)
    }
    fn new_capped(selection: JitSelection, extra: bool, cap: Option<u64>) -> Self {
        let collect = Arc::new(AtomicBool::new(false));
        let records = Arc::new(Mutex::new(Vec::new()));
        let trace = Arc::new(Mutex::new(Trace::default()));
        let mut builder = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .extension_installer(installer(collect.clone(), records.clone(), cap.is_some()));
        if let Some(cap) = cap {
            builder = builder.max_heap_bytes(cap);
        }
        let mut runtime = builder.build().unwrap();
        runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
        let realm = extra.then(|| runtime.create_realm().unwrap());
        let mut fixture = Self {
            runtime,
            realm,
            selection,
            sequence: 0,
            artifacts: BTreeMap::new(),
            events: Vec::new(),
            trace,
            collect,
            records,
        };
        fixture.setup(&format!(
            "{DEFINITIONS}\nfor(let warm=0;warm<1000;warm++){{{WARM}}}"
        ));
        fixture
    }
    fn run(&mut self, source: &str, module: &str) -> otter_runtime::ExecutionResult {
        let source = SourceInput::from_javascript(source);
        match self.realm {
            Some(realm) => self.runtime.run_script_in_realm(realm, source, module),
            None => self.runtime.run_script(source, module),
        }
        .unwrap()
    }
    fn setup(&mut self, source: &str) {
        let name = if self.sequence == 0 {
            MODULE.to_owned()
        } else {
            format!("known-native-warm-{}.js", self.sequence)
        };
        self.sequence += 1;
        let output = self.run(source, &name);
        let artifacts = output.jit_artifacts().unwrap();
        assert!(!artifacts.truncated());
        let report = output.jit_debug_report().unwrap();
        assert!(!report.truncated());
        assert_eq!(report.dropped_events(), 0);
        self.events.extend(report.events().iter().cloned());
        for bundle in artifacts.bundles() {
            self.artifacts
                .insert(bundle.manifest().code_object_id(), bundle.clone());
        }
    }
    fn current(&self, name: &str) -> Option<JitCodeGenerationSnapshot> {
        self.current_in_module(name, MODULE)
    }
    fn current_in_module(&self, name: &str, module: &str) -> Option<JitCodeGenerationSnapshot> {
        let tier = match self.selection {
            JitSelection::Template => NativeFrameKind::Baseline,
            JitSelection::ProductionTiered => NativeFrameKind::Optimizing,
            JitSelection::InterpreterOnly => return None,
        };
        let found: Vec<_> = self
            .runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .filter(|generation| {
                generation.tier == tier
                    && generation.lifecycle == CodeLifetimeState::Installed
                    && generation.linked
                    && self
                        .artifacts
                        .get(&generation.code_object_id)
                        .is_some_and(|bundle| {
                            bundle.manifest().module() == module
                                && bundle.manifest().function_name() == name
                                && bundle.manifest().entry() == JitDebugTarget::Entry
                        })
            })
            .collect();
        assert!(found.len() <= 1, "one current {name} generation");
        found.into_iter().next()
    }
    fn admit(&mut self) {
        for batch in 0..=128 {
            if NAMES.iter().all(|name| self.current(name).is_some()) {
                return;
            }
            assert!(
                batch < 128,
                "normal {:?} admission exhausted: generations={:?}; ownArtifacts={:?}; events={:?}",
                self.selection,
                self.runtime.jit_code_generation_snapshot(),
                self.artifacts
                    .values()
                    .filter(|bundle| bundle.manifest().module() == MODULE)
                    .map(|bundle| bundle.manifest())
                    .collect::<Vec<_>>(),
                self.events.iter().rev().take(32).collect::<Vec<_>>()
            );
            self.setup(&WARM.repeat(16));
        }
    }
}
fn json(bundle: &JitArtifactBundle, file: JitArtifactFileName) -> Json {
    serde_json::from_slice(bundle.file(file).unwrap().contents()).unwrap()
}
fn assert_native_edge(bundle: &JitArtifactBundle) {
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let metadata = return_sites::assert_sites(bundle);
    let links: Vec<_> = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|link| {
            link["target"]["kind"] == "runtimeStub"
                && link["target"]["id"].as_u64() == Some(u64::from(abi::STUB_JIT_CALL_NATIVE.id))
        })
        .collect();
    assert_eq!(
        links.len(),
        1,
        "one own Native entry in {}",
        bundle.manifest().function_name()
    );
    let link = links[0];
    assert_eq!(link["target"]["name"], "jit_call_native");
    assert_eq!(link["target"]["signature"], "jsCall");
    let end = link["endOffset"].as_u64().unwrap() as usize;
    #[cfg(target_arch = "aarch64")]
    let call: &[u8] = &[0x00, 0x02, 0x3f, 0xd6];
    #[cfg(target_arch = "x86_64")]
    let call: &[u8] = &[0x41, 0xff, 0xd3];
    assert_eq!(code.get(end..end + call.len()), Some(call));
    let offset = end + call.len();
    let sites: Vec<_> = metadata["returnSites"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|site| site["nativeReturnOffset"].as_u64() == Some(offset as u64))
        .collect();
    assert_eq!(sites.len(), 1);
    let record = metadata["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == sites[0]["safepointId"])
        .unwrap();
    let regions: Vec<_> = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["startOffset"]
                    .as_u64()
                    .is_some_and(|start| start <= end as u64)
                && region["endOffset"]
                    .as_u64()
                    .is_some_and(|finish| offset as u64 <= finish)
        })
        .collect();
    assert_eq!(regions.len(), 1);
    let region = regions[0];
    assert_eq!(
        region["functionId"].as_u64(),
        Some(u64::from(bundle.manifest().function_id()))
    );
    assert_eq!(region["logicalPc"], record["callPc"]);
    assert!(region["bytePc"].as_u64().is_some());
    assert!(record["inlineFrames"].as_array().unwrap().is_empty());
    assert!(!record["taggedLocations"].as_array().unwrap().is_empty());
    if matches!(
        bundle.manifest().function_name(),
        "knownNativeZero" | "knownNativeOne" | "knownNativeOdd"
    ) {
        return_sites::assert_plain_hot_source_unstamped(
            code,
            region["startOffset"].as_u64().unwrap() as usize,
            offset,
        );
    }
}
fn assert_source_and_lease(
    source: &str,
    generations: &[JitCodeGenerationSnapshot],
    own: &JitCodeGenerationSnapshot,
) {
    let frames: Json = serde_json::from_str(source).unwrap();
    let found: Vec<_> = frames
        .as_array()
        .unwrap()
        .iter()
        .filter(|frame| frame["functionName"] == "knownNativeOdd")
        .collect();
    assert_eq!(found.len(), 1, "exact suspended source: {frames}");
    assert!(found[0]["scriptName"].as_str().unwrap().ends_with(MODULE));
    assert_eq!(found[0]["sourceLine"], "  const result = fn(a, b, c);");
    let current: Vec<_> = generations
        .iter()
        .filter(|generation| {
            generation.code_object_id == own.code_object_id
                && generation.function_id == own.function_id
                && generation.tier == own.tier
        })
        .collect();
    assert_eq!(current.len(), 1);
    let current = current[0];
    assert_eq!(current.lifecycle, CodeLifetimeState::Installed);
    assert!(current.linked);
    assert_eq!(current.active_count, 1);
    assert_eq!(current.generated_deopts, own.generated_deopts);
}
#[test]
fn selected_native_live_target_roots_and_policy_match_interpreter() {
    const PROBE: &str = r#"
let knownConstructRejected, knownThrowResult;
try { knownNativeConstruct(nativeKindB,nativeWarmB); }
catch (error) { knownConstructRejected = error instanceof TypeError; }
try { knownNativeThrow(nativeKindThrow); }
catch (error) { knownThrowResult = [error.name,error.message.includes('native-kind-throw')]; }
const knownLiveA = nativeKindRenew(41), knownLiveB = nativeKindRenew(42);
JSON.stringify([knownNativeOdd(nativeKindB,knownLiveA,knownLiveB,knownLiveA),
 knownNativeZero(nativeKindB),knownNativeOne(nativeKindB,knownLiveB),
 knownNativeOdd(nativeKindA,knownLiveA,knownLiveB,knownLiveA),
 knownNativeConstruct(Array,knownLiveA),knownConstructRejected,
 knownThrowResult,knownLiveA !== knownLiveB]);
"#;
    const EXPECTED: &str = "[[1003,41,42,41,true],1000,1001,[3,41,42,41,true],[1,true],true,[\"TypeError\",true],true]";
    // All three misses enter the existing Generic path once. The replacement
    // requires its actual-argument activation; Proxy effects cannot be replayed.
    const MISS: &str = r#"
let knownProxyEffects = 0;
const knownNativeProxy = new Proxy(nativeKindB, {apply(target, receiver, args) {
  knownProxyEffects++; return Reflect.apply(target, receiver, args);
}});
let knownNonCallable;
try { knownNativeZero(17); } catch (error) { knownNonCallable = error instanceof TypeError; }
JSON.stringify([knownNativeOdd(knownNativeReplacement,nativeWarmA,nativeWarmB,nativeWarmA),
 knownNativeOdd(knownNativeProxy,nativeWarmA,nativeWarmB,nativeWarmA),knownNonCallable,knownProxyEffects]);
"#;
    const MISS_EXPECTED: &str = "[[2003,41,42,41,true],[1003,41,42,41,true],true,1]";
    for extra in [false, true] {
        let mut oracle = Fixture::new(JitSelection::InterpreterOnly, extra);
        assert_eq!(
            oracle
                .run(PROBE, "known-native-oracle.js")
                .completion_string(),
            EXPECTED
        );
        assert_eq!(
            oracle
                .run(MISS, "known-native-miss-oracle.js")
                .completion_string(),
            MISS_EXPECTED
        );
        for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
            let mut fixture = Fixture::new(selection, extra);
            fixture.admit();
            let before: Vec<_> = NAMES
                .iter()
                .map(|name| fixture.current(name).unwrap())
                .collect();
            for own in &before {
                assert_native_edge(&fixture.artifacts[&own.code_object_id]);
                let bundle = &fixture.artifacts[&own.code_object_id];
                println!(
                    "selection={selection:?} additionalRealm={extra}; own={} codeId={} codeBytes={}; staticBytesOnly=true",
                    bundle.manifest().function_name(),
                    own.code_object_id,
                    bundle
                        .file(JitArtifactFileName::Code)
                        .unwrap()
                        .contents()
                        .len()
                );
            }
            fixture.collect.store(true, Ordering::Relaxed);
            fixture.trace.lock().unwrap().recording = true;
            let stats = fixture.runtime.execution_stats();
            let result = fixture.run(PROBE, "known-native-probe.js");
            fixture.trace.lock().unwrap().recording = false;
            assert_eq!(result.completion_string(), EXPECTED);
            assert!(fixture.runtime.execution_stats().gc_cycles > stats.gc_cycles);
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
                "own Native probe has no compilation/exit: {:?}",
                report.events()
            );
            let trace = fixture.trace.lock().unwrap();
            assert!(trace.total > 0);
            assert_eq!(trace.total, trace.counts.values().sum::<usize>());
            for (name, old) in NAMES.iter().zip(&before) {
                assert_eq!(
                    trace.counts.get(&old.function_id).copied().unwrap_or(0),
                    0,
                    "native {name}"
                );
                let now = fixture.current(name).unwrap();
                assert_eq!(now.code_object_id, old.code_object_id);
                assert_eq!(now.generated_deopts, old.generated_deopts);
            }
            drop(trace);
            let records = fixture.records.lock().unwrap();
            assert_eq!(
                records.len(),
                1,
                "one actual dynamic callback and collection"
            );
            let record = records[0]
                .as_ref()
                .unwrap_or_else(|error| panic!("{error}"));
            let odd = before
                .iter()
                .find(|own| {
                    fixture.artifacts[&own.code_object_id]
                        .manifest()
                        .function_name()
                        == "knownNativeOdd"
                })
                .unwrap();
            assert_source_and_lease(&record.before, &record.generations_before, odd);
            assert_source_and_lease(&record.after, &record.generations_after, odd);
            let [a, b, c] = record.children.as_slice() else {
                panic!("three retained actuals")
            };
            assert_eq!(a.before, c.before);
            assert_eq!(a.after, c.after);
            assert_ne!(a.before, b.before);
            assert_ne!(b.before, b.after, "last fresh actual really moves");
            assert_eq!(
                [a.marker_before, b.marker_before, c.marker_before],
                [41.0, 42.0, 41.0]
            );
            assert_eq!(
                [a.marker_after, b.marker_after, c.marker_after],
                [41.0, 42.0, 41.0]
            );
            drop(records);
            fixture.collect.store(false, Ordering::Relaxed);
            let miss = fixture.run(MISS, "known-native-miss-probe.js");
            assert_eq!(miss.completion_string(), MISS_EXPECTED);
            let report = miss.jit_debug_report().unwrap();
            assert!(!report.truncated());
            assert_eq!(report.dropped_events(), 0);
            println!(
                "selection={selection:?} additionalRealm={extra}; spans={:?}; staticBytesOnly=true",
                abi::native_entry_code_sizes()
            );
        }
    }
}
