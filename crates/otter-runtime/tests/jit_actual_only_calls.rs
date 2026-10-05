//! Actual-only argument spans through current installed JavaScript cells.
//!
//! # Contents
//! - Underarity, overarity, mapped arguments, defaults and rest parameters.
//! - Forwarded arguments and proper tails with zero, fitting and growing spans.
//! - Underarity construction and full collection inside a generated callee.
//! - Nested bound prefixes and default objects in default and source realms.
//! - Fresh children reached through actual payloads move in the collecting call;
//!   admitted bound prefix shells and callable identities may already be old.
//!
//! # Invariants
//! - Ordinary source work admits every subject; tests never force a tier or budget.
//! - Exact current artifacts prove Known edges, and complete isolated traces
//!   exclude interpreted subject execution during the measured calls.
//! - Probe generations remain installed and linked with no compile, OSR or exit.
//! - A handle scope roots exact fresh children across a real full collection.
//!
//! # See also
//! - `otter_jit::entry::actual_arguments_tests` for poisoned native alignment slack.
//! - `jit_forward_arguments` for mutated forwarded windows and exceptional calls.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use otter_bytecode::Op;
use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitSelection, Runtime,
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeRealmId, RuntimeValue,
    SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{CodeLifetimeState, NativeFrameKind},
};

#[path = "support/moving_children.rs"]
mod moving_children;
#[path = "support/return_sites.rs"]
mod return_sites;

const DEFINITIONS: &str = r#"
function actualEcho(a, b, c) {
  return [arguments.length, a, b, c, arguments[0], arguments[3]];
}
function actualMapped(a, b, c) {
  a = 7; b = 8;
  return [arguments.length, arguments[0], arguments[1], c];
}
function actualDefaults(a = {marker: 41}, b = 2, ...rest) {
  return [arguments.length, a.marker, b, rest];
}
function actualZero() {
  if (arguments.length !== 0) throw "zero caller";
  const result = actualEcho(); return result;
}
function actualOne(a) {
  if (arguments.length !== 1) throw "one caller";
  const result = actualEcho(a); return result;
}
function actualMany(a, b, c, d, e) {
  if (arguments.length !== 5) throw "many caller";
  const result = actualEcho(a, b, c, d, e); return result;
}
function actualMappedOne(a) {
  if (arguments.length !== 1) throw "mapped caller";
  const result = actualMapped(a); return result;
}
function actualDefaultsZero() {
  if (arguments.length !== 0) throw "default caller";
  const result = actualDefaults(); return result;
}
function actualDefaultsMany(a) {
  if (arguments.length !== 1) throw "rest caller";
  const result = actualDefaults(a, 8, 9, 10, 11); return result;
}
function actualForward(a, b, c) {
  a = 9;
  const result = actualEcho.apply(undefined, arguments); return result;
}
function actualForwardBridge(a) {
  if (arguments.length !== 1) throw "forward bridge";
  const result = actualForward(a); return result;
}
function actualTail(a, b, c) {
  "use strict";
  if (arguments.length !== 1) throw "tail arguments";
  return actualEcho(a);
}
function actualTailZero() {
  "use strict";
  if (arguments.length !== 0) throw "zero tail arguments";
  return actualEcho();
}
function actualTailGrow() {
  "use strict";
  if (arguments.length !== 0) throw "growing tail arguments";
  return actualEcho(1, 2, 3, 4, 5);
}
function actualTailBridge(a) {
  if (arguments.length !== 1) throw "tail bridge";
  const result = actualTail(a); return result;
}
function actualTailZeroBridge() {
  if (arguments.length !== 0) throw "zero tail bridge";
  const result = actualTailZero(); return result;
}
function actualTailGrowBridge() {
  if (arguments.length !== 0) throw "growing tail bridge";
  const result = actualTailGrow(); return result;
}
function ActualUnderConstructor(value, missing) {
  this.value = value; this.missing = missing;
}
function ActualCollectConstructor(value, missing) {
  if (arguments.length !== 1) throw "constructor arguments";
  this.value = value;
  const child = actualRenewChild(value);
  actualCollect(child);
  this.marker = child.marker; this.missing = missing; this.child = child;
  if (value.child !== child) throw "constructor child alias";
}
function actualConstruct(value) {
  if (arguments.length !== 1) throw "construct bridge";
  const result = new ActualUnderConstructor(value); return result;
}
function actualConstructCollect(value) {
  if (arguments.length !== 1) throw "collect bridge";
  const result = new ActualCollectConstructor(value); return result;
}
const ActualBoundConstructor = ActualUnderConstructor.bind(null);
function actualBoundConstruct(value) {
  if (arguments.length !== 1) throw "bound bridge";
  const result = new ActualBoundConstructor(value); return result;
}
function actualBoundTarget(a, b, c, missing) {
  const child = actualRenewChild(a);
  actualCollect(child);
  return [arguments.length, a.marker, b, c, missing, Object.getPrototypeOf(a) === Object.prototype,
    child.marker, a.child === child];
}
const actualBoundPayload = {marker:45};
const actualBoundFirst = actualBoundTarget.bind(null, actualBoundPayload);
const actualBoundSecond = actualBoundFirst.bind(null, 7);
function actualBoundCall(value) {
  if (arguments.length !== 1) throw "bound call arguments";
  const result = actualBoundSecond(value); return result;
}
const ActualBoundCollectConstructor = ActualCollectConstructor.bind(null, {marker:46});
function actualBoundConstructCollect() {
  if (arguments.length !== 0) throw "bound collecting constructor arguments";
  const result = new ActualBoundCollectConstructor(); return result;
}
function actualDefaultCollect(a = {marker:47}, missingA, missingB) {
  if (arguments.length !== 0) throw "collecting default arguments";
  const child = actualRenewChild(a);
  actualCollect(child);
  return [arguments.length, a.marker, missingA, missingB, Object.getPrototypeOf(a) === Object.prototype,
    child.marker, a.child === child];
}
function actualDefaultCollectBridge() {
  if (arguments.length !== 0) throw "collecting default bridge arguments";
  const result = actualDefaultCollect(); return result;
}
function actualTailCollectTarget(payload, missing) {
  const child = actualRenewChild(payload);
  actualCollect(child);
  return [child.marker, missing, child === payload.child];
}
function actualTailCollect(payload) {
  "use strict";
  return actualTailCollectTarget(payload);
}
function actualTailCollectBridge(payload) {
  const result = actualTailCollect(payload); return result;
}
function actualThrowCollect(payload) {
  const child = actualRenewChild(payload);
  actualCollect(child);
  throw child;
}
function actualThrowCollectBridge(payload) {
  const result = actualThrowCollect(payload); return result;
}
"#;

const CALLS: &str = r#"
actualZero(); actualOne(5); actualMany(1,2,3,4,5); actualMappedOne(5);
actualDefaultsZero(); actualDefaultsMany({marker:7});
actualForwardBridge(5); actualTailBridge(5); actualTailZeroBridge(); actualTailGrowBridge();
actualConstruct({marker:42}); actualConstructCollect({marker:42}); actualBoundConstruct({marker:42});
actualBoundCall(8); actualBoundConstructCollect(); actualDefaultCollectBridge();
actualTailCollectBridge({marker:51});
try { actualThrowCollectBridge({marker:52}); } catch (error) { if (error.marker !== 52) throw error; }
"#;

#[derive(Default)]
struct Trace {
    recording: bool,
    ticks: Vec<(u32, String, Op)>,
    dropped: usize,
}
struct Tracer(Arc<Mutex<Trace>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self.0.lock().unwrap();
        if !trace.recording {
            return;
        }
        if trace.ticks.len() == 16_384 {
            trace.dropped += 1;
            return;
        }
        trace
            .ticks
            .push((event.function_id, event.function_name.to_owned(), event.op));
    }
}

struct Fixture {
    runtime: Runtime,
    selection: JitSelection,
    setup_sequence: usize,
    realm: Option<RuntimeRealmId>,
    artifacts: BTreeMap<u64, JitArtifactBundle>,
    trace: Arc<Mutex<Trace>>,
    collect: Arc<AtomicBool>,
    collections: Arc<AtomicUsize>,
    observations:
        Arc<Mutex<Vec<Result<(Vec<moving_children::ChildMotion>, String, String), String>>>>,
}

impl Fixture {
    fn new(selection: JitSelection) -> Self {
        Self::new_in_realm(selection, false)
    }

    fn new_in_realm(selection: JitSelection, extra_realm: bool) -> Self {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let collect = Arc::new(AtomicBool::new(false));
        let collections = Arc::new(AtomicUsize::new(0));
        let observations = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .extension_installer(RuntimeExtensionInstaller::new({
                let collect = collect.clone();
                let collections = collections.clone();
                let observations = observations.clone();
                move |realm| {
                    let collect = collect.clone();
                    let collections = collections.clone();
                    let observations = observations.clone();
                    moving_children::install(realm)?;
                    realm.install_native_global_call(
                        "actualRenewChild",
                        1,
                        RuntimeNativeCall::Dynamic(Arc::new(
                            |ctx: &mut RuntimeNativeCtx<'_>,
                             args: &[RuntimeValue],
                             _state: &[RuntimeValue]| {
                                ctx.scope(|mut scope| {
                                    let payload = scope.argument(args, 0);
                                    let marker = scope.get(payload, "marker")?;
                                    let layout = scope.object_layout(&["marker"])?;
                                    let child = scope.object_with_layout(layout, &[marker])?;
                                    scope.set(payload, "child", child)?;
                                    Ok(scope.finish(child))
                                })
                            },
                        )),
                    )?;
                    realm.install_native_global_call(
                        "actualCollect",
                        1,
                        RuntimeNativeCall::Dynamic(Arc::new(
                            move |ctx: &mut RuntimeNativeCtx<'_>,
                                  args: &[RuntimeValue],
                                  _state: &[RuntimeValue]| {
                                if collect.load(Ordering::Relaxed) {
                                    let before = ctx
                                        .execution_context()
                                        .map(|context| ctx.capture_call_sites_json(context, 0, 64));
                                    let moved = moving_children::observe_and_collect(ctx, args);
                                    let after = ctx
                                        .execution_context()
                                        .map(|context| ctx.capture_call_sites_json(context, 0, 64));
                                    let observation = match (moved, before, after) {
                                        (Ok(moved), Some(before), Some(after)) => {
                                            Ok((moved, before, after))
                                        }
                                        (Err(error), _, _) => {
                                            Err(format!("moving child observation: {error:?}"))
                                        }
                                        _ => {
                                            Err("collecting callback has no active source context"
                                                .to_owned())
                                        }
                                    };
                                    observations
                                        .lock()
                                        .map_err(|_| otter_runtime::RuntimeNativeError::Error {
                                            message: "actual observation recorder poisoned"
                                                .to_owned(),
                                        })?
                                        .push(observation);
                                    collections.fetch_add(1, Ordering::Relaxed);
                                }
                                Ok(RuntimeValue::undefined())
                            },
                        )),
                    )
                }
            }))
            .build()
            .unwrap();
        runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
        let realm = extra_realm.then(|| runtime.create_realm().unwrap());
        let mut fixture = Self {
            runtime,
            selection,
            setup_sequence: 0,
            realm,
            artifacts: BTreeMap::new(),
            trace,
            collect,
            collections,
            observations,
        };
        fixture.setup(&format!(
            "{DEFINITIONS}\nfor(let warm=0;warm<1500;warm++){{{CALLS}}}"
        ));
        fixture
    }

    fn setup(&mut self, source: &str) {
        // Preserve the original source URL: later warm mains must not replace
        // its text while retained subject functions still own those spans.
        let module = if self.setup_sequence == 0 {
            "actual-only-setup.js".to_owned()
        } else {
            format!("actual-only-warm-{}.js", self.setup_sequence)
        };
        self.setup_sequence += 1;
        let result = self.run(source, &module);
        let artifacts = result.jit_artifacts().unwrap();
        assert!(!artifacts.truncated());
        for bundle in artifacts.bundles() {
            self.artifacts
                .insert(bundle.manifest().code_object_id(), bundle.clone());
        }
    }

    fn run(&mut self, source: &str, module: &str) -> otter_runtime::ExecutionResult {
        let source = SourceInput::from_javascript(source);
        match self.realm {
            Some(realm) => self.runtime.run_script_in_realm(realm, source, module),
            None => self.runtime.run_script(source, module),
        }
        .unwrap()
    }

    fn current(&self, name: &str) -> Option<JitCodeGenerationSnapshot> {
        self.runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .filter(|generation| {
                generation.lifecycle == CodeLifetimeState::Installed
                    && generation.linked
                    && self
                        .artifacts
                        .get(&generation.code_object_id)
                        .is_some_and(|bundle| bundle.manifest().function_name() == name)
            })
            .max_by_key(|generation| {
                (
                    generation.tier == NativeFrameKind::Optimizing,
                    generation.code_object_id,
                )
            })
    }

    fn admit(&mut self, names: &[&str]) {
        for batch in 0..=128 {
            if names.iter().all(|name| {
                self.current(name).is_some_and(|generation| {
                    generation.tier
                        == match self.selection {
                            JitSelection::ProductionTiered => NativeFrameKind::Optimizing,
                            JitSelection::Template => NativeFrameKind::Baseline,
                            JitSelection::InterpreterOnly => unreachable!("native fixture"),
                        }
                })
            }) {
                return;
            }
            if batch == 128 {
                break;
            }
            // Fresh straight-line mains cannot absorb subject work into their own OSR.
            self.setup(&CALLS.repeat(16));
        }
        panic!(
            "ordinary {:?} admission exhausted for {names:?}; generations={:?}; artifacts={:?}",
            self.selection,
            self.runtime.jit_code_generation_snapshot(),
            self.artifacts
                .values()
                .map(|bundle| (
                    bundle.manifest().function_name(),
                    bundle.manifest().code_object_id()
                ))
                .collect::<Vec<_>>()
        );
    }

    fn edge(&self, caller: &str, callee: &str) {
        let caller = self.current(caller).unwrap();
        let callee = self.current(callee).unwrap();
        let bundle = &self.artifacts[&caller.code_object_id];
        assert_eq!(bundle.manifest().function_id(), caller.function_id);
        let relocations: serde_json::Value = serde_json::from_slice(
            bundle
                .file(JitArtifactFileName::Relocations)
                .unwrap()
                .contents(),
        )
        .unwrap();
        assert!(
            relocations["relocations"].as_array().unwrap().iter().any(
                |relocation| relocation["target"]["kind"] == "functionEntryCell"
                    && relocation["target"]["functionId"].as_u64()
                        == Some(u64::from(callee.function_id))
            ),
            "exact current caller {caller:?} must emit Known linkage to {callee:?}; source={}; relocations={relocations}",
            bundle.manifest().function_name()
        );
        return_sites::assert_known(bundle, callee.function_id);
    }

    fn assert_collections(&self, expected: usize, caller: &[(&str, &str)]) {
        let observations = self.observations.lock().unwrap();
        assert_eq!(observations.len(), expected);
        for observation in observations.iter() {
            let (motion, before, after) = observation.as_ref().expect("owned native observation");
            assert!(!motion.is_empty());
            for child in motion {
                assert_ne!(
                    child.before, child.after,
                    "actual young child moves in the measured collection"
                );
                assert_eq!(child.marker_before, child.marker_after);
            }
            let before: serde_json::Value = serde_json::from_str(before).unwrap();
            let after: serde_json::Value = serde_json::from_str(after).unwrap();
            assert_eq!(
                before, after,
                "the same exact suspended call sources survive moving GC"
            );
            let frames = before.as_array().unwrap();
            for (name, source) in caller {
                let matching: Vec<_> = frames
                    .iter()
                    .filter(|frame| frame["functionName"] == *name)
                    .collect();
                if matching.is_empty() {
                    continue;
                }
                assert_eq!(matching.len(), 1, "one physical source owner for {name}");
                assert_eq!(matching[0]["scriptName"], "actual-only-setup.js");
                assert_eq!(matching[0]["sourceLine"].as_str().unwrap().trim(), *source);
            }
            assert!(
                caller
                    .iter()
                    .any(|(name, _)| frames.iter().any(|frame| frame["functionName"] == *name)),
                "each collection observes its own suspended generated bridge"
            );
        }
        for (name, _) in caller {
            assert!(
                observations.iter().any(|observation| {
                    let (_, source, _) = observation.as_ref().unwrap();
                    let frames: serde_json::Value = serde_json::from_str(source).unwrap();
                    frames
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|frame| frame["functionName"] == *name)
                }),
                "actual source owner {name} must be observed"
            );
            let current = self.current(name).unwrap();
            return_sites::assert_sites(&self.artifacts[&current.code_object_id]);
        }
    }

    fn probe(&mut self, source: &str, names: &[&str], expected: &str) {
        self.admit(names);
        let before: Vec<_> = names
            .iter()
            .map(|name| self.current(name).unwrap())
            .collect();
        {
            let mut trace = self.trace.lock().unwrap();
            trace.ticks.clear();
            trace.dropped = 0;
            trace.recording = true;
        }
        let result = self.run(source, "actual-only-probe.js");
        self.trace.lock().unwrap().recording = false;
        assert_eq!(result.completion_string(), expected);
        let events = result.jit_debug_report().unwrap();
        assert!(!events.truncated());
        assert_eq!(events.dropped_events(), 0);
        assert!(
            !events.events().iter().any(|event| matches!(
                event,
                JitDebugEvent::CompilePrepared { .. }
                    | JitDebugEvent::Bail { .. }
                    | JitDebugEvent::EnteredGenerationDeopt { .. }
                    | JitDebugEvent::InlineDeoptFrame { .. }
            )),
            "isolated current calls have no compilation or exits: {:?}",
            events.events()
        );
        let trace = self.trace.lock().unwrap();
        assert_eq!(trace.dropped, 0);
        assert!(
            trace
                .ticks
                .iter()
                .any(|(_, _, op)| matches!(op, Op::Call | Op::New))
        );
        assert!(
            trace
                .ticks
                .iter()
                .any(|(_, _, op)| matches!(op, Op::ReturnValue | Op::Return))
        );
        for (name, previous) in names.iter().zip(before) {
            let current = self.current(name).unwrap();
            assert_eq!(
                current.code_object_id, previous.code_object_id,
                "same current {name}"
            );
            assert_eq!(current.generated_deopts, previous.generated_deopts);
            assert!(
                !trace
                    .ticks
                    .iter()
                    .any(|(fid, _, _)| *fid == previous.function_id),
                "{name} must execute its current native body: {:?}",
                trace.ticks
            );
        }
    }
}

#[test]
fn installed_known_calls_preserve_missing_extra_default_and_rest_arguments() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        installed_known_calls(selection);
    }
}

fn installed_known_calls(selection: JitSelection) {
    let names = [
        "actualEcho",
        "actualMapped",
        "actualDefaults",
        "actualZero",
        "actualOne",
        "actualMany",
        "actualMappedOne",
        "actualDefaultsZero",
        "actualDefaultsMany",
    ];
    let mut fixture = Fixture::new(selection);
    fixture.admit(&names);
    for (caller, callee) in [
        ("actualZero", "actualEcho"),
        ("actualOne", "actualEcho"),
        ("actualMany", "actualEcho"),
        ("actualMappedOne", "actualMapped"),
        ("actualDefaultsZero", "actualDefaults"),
        ("actualDefaultsMany", "actualDefaults"),
    ] {
        fixture.edge(caller, callee);
    }
    fixture.probe("JSON.stringify([actualZero(),actualOne(5),actualMany(1,2,3,4,5),actualMappedOne(5),actualDefaultsZero(),actualDefaultsMany({marker:7})]);",
        &names, "[[0,null,null,null,null,null],[1,5,null,null,5,null],[5,1,2,3,1,4],[1,7,null,null],[0,41,2,[]],[5,7,8,[9,10,11]]]");
}

#[test]
fn actual_only_forwarded_and_tail_spans_fit_grow_and_keep_zero_arity() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        forwarded_and_tail_spans(selection);
    }
}

fn forwarded_and_tail_spans(selection: JitSelection) {
    let names = [
        "actualEcho",
        "actualForward",
        "actualForwardBridge",
        "actualTail",
        "actualTailZero",
        "actualTailGrow",
        "actualTailBridge",
        "actualTailZeroBridge",
        "actualTailGrowBridge",
    ];
    let mut fixture = Fixture::new(selection);
    fixture.admit(&names);
    for (caller, callee) in [
        ("actualForwardBridge", "actualForward"),
        ("actualTailBridge", "actualTail"),
        ("actualTailZeroBridge", "actualTailZero"),
        ("actualTailGrowBridge", "actualTailGrow"),
        ("actualTail", "actualEcho"),
        ("actualTailZero", "actualEcho"),
        ("actualTailGrow", "actualEcho"),
    ] {
        fixture.edge(caller, callee);
    }
    // The production policy admits Graph on both native architectures; the
    // exact optimizing caller must emit this actual-only Known forward edge.
    if selection == JitSelection::ProductionTiered {
        fixture.edge("actualForward", "actualEcho");
    }
    fixture.probe("JSON.stringify([actualForwardBridge(5),actualTailBridge(5),actualTailZeroBridge(),actualTailGrowBridge()]);",
        &names, "[[1,9,null,null,9,null],[1,5,null,null,5,null],[0,null,null,null,null,null],[5,1,2,3,1,4]]");
}

#[test]
fn underarity_constructor_windows_survive_collection_and_bound_classification() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        constructor_windows(selection);
    }
}

fn constructor_windows(selection: JitSelection) {
    let names = [
        "ActualUnderConstructor",
        "ActualCollectConstructor",
        "actualConstruct",
        "actualConstructCollect",
        "actualBoundConstruct",
    ];
    let mut fixture = Fixture::new(selection);
    fixture.admit(&names);
    fixture.edge("actualConstruct", "ActualUnderConstructor");
    fixture.edge("actualConstructCollect", "ActualCollectConstructor");
    fixture.collect.store(true, Ordering::Relaxed);
    let before = fixture.runtime.execution_stats();
    fixture.probe("const plain=actualConstruct({marker:42});const collected=actualConstructCollect({marker:43});const bound=actualBoundConstruct({marker:44});JSON.stringify([plain.value.marker,plain.missing,collected.value.marker,collected.marker,collected.missing,bound.value.marker,bound.missing,collected.child.marker,collected.value.child===collected.child]);",
        &names, "[42,null,43,43,null,44,null,43,true]");
    assert_eq!(fixture.collections.load(Ordering::Relaxed), 1);
    fixture.assert_collections(
        1,
        &[(
            "actualConstructCollect",
            "const result = new ActualCollectConstructor(value); return result;",
        )],
    );
    assert!(
        fixture.runtime.execution_stats().gc_cycles > before.gc_cycles,
        "real full collection occurs while initialized underarity arguments are live"
    );
}

#[test]
fn actual_only_bound_prefixes_and_default_objects_survive_collection_in_source_realms() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        for extra_realm in [false, true] {
            let names = [
                "actualBoundTarget",
                "actualBoundCall",
                "ActualCollectConstructor",
                "actualBoundConstructCollect",
                "actualDefaultCollect",
                "actualDefaultCollectBridge",
            ];
            let mut fixture = Fixture::new_in_realm(selection, extra_realm);
            fixture.admit(&names);
            fixture.edge("actualDefaultCollectBridge", "actualDefaultCollect");
            fixture.collect.store(true, Ordering::Relaxed);
            let before = fixture.runtime.execution_stats();
            fixture.probe("const boundCall=actualBoundCall(8);const boundConstruct=actualBoundConstructCollect();const defaultCall=actualDefaultCollectBridge();JSON.stringify([boundCall,boundConstruct.value.marker,boundConstruct.marker,boundConstruct.missing,Object.getPrototypeOf(boundConstruct)===ActualCollectConstructor.prototype,boundConstruct.child.marker,boundConstruct.value.child===boundConstruct.child,defaultCall]);",
                &names, "[[3,45,7,8,null,true,45,true],46,46,null,true,46,true,[0,47,null,null,true,47,true]]");
            assert_eq!(fixture.collections.load(Ordering::Relaxed), 3);
            fixture.assert_collections(
                3,
                &[
                    (
                        "actualBoundCall",
                        "const result = actualBoundSecond(value); return result;",
                    ),
                    (
                        "actualBoundConstructCollect",
                        "const result = new ActualBoundCollectConstructor(); return result;",
                    ),
                    (
                        "actualDefaultCollectBridge",
                        "const result = actualDefaultCollect(); return result;",
                    ),
                ],
            );
            let after = fixture.runtime.execution_stats();
            assert!(after.gc_cycles >= before.gc_cycles + 3);
            assert!(
                after.gc_minor_slot_updates > before.gc_minor_slot_updates,
                "collection rewrites live young values across actual-only calls"
            );
        }
    }
}

#[test]
fn proper_tail_and_throwing_children_keep_their_exact_suspended_sources_through_gc() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let names = [
            "actualTailCollectTarget",
            "actualTailCollect",
            "actualTailCollectBridge",
            "actualThrowCollect",
            "actualThrowCollectBridge",
        ];
        let mut fixture = Fixture::new(selection);
        fixture.admit(&names);
        fixture.edge("actualTailCollectBridge", "actualTailCollect");
        fixture.edge("actualTailCollect", "actualTailCollectTarget");
        fixture.edge("actualThrowCollectBridge", "actualThrowCollect");
        fixture.collect.store(true, Ordering::Relaxed);
        let before = fixture.runtime.execution_stats();
        fixture.probe("const tailResult=actualTailCollectBridge({marker:51});const throwPayload={marker:52};let throwResult;try{actualThrowCollectBridge(throwPayload);}catch(error){throwResult=[error.marker,error===throwPayload.child];}JSON.stringify([tailResult,throwResult]);",
            &names, "[[51,null,true],[52,true]]");
        assert_eq!(fixture.collections.load(Ordering::Relaxed), 2);
        fixture.assert_collections(
            2,
            &[
                (
                    "actualTailCollectBridge",
                    "const result = actualTailCollect(payload); return result;",
                ),
                (
                    "actualThrowCollectBridge",
                    "const result = actualThrowCollect(payload); return result;",
                ),
            ],
        );
        let observations = fixture.observations.lock().unwrap();
        let tail = observations
            .iter()
            .find(|observation| {
                let (_, source, _) = observation.as_ref().unwrap();
                let frames: serde_json::Value = serde_json::from_str(source).unwrap();
                frames
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|frame| frame["functionName"] == "actualTailCollectTarget")
            })
            .unwrap();
        let (_, source, _) = tail.as_ref().unwrap();
        let frames: serde_json::Value = serde_json::from_str(source).unwrap();
        assert!(
            !frames
                .as_array()
                .unwrap()
                .iter()
                .any(|frame| frame["functionName"] == "actualTailCollect"),
            "the proper tail removed its physical caller; the collecting target inherited only the bridge return"
        );
        assert!(fixture.runtime.execution_stats().gc_cycles >= before.gc_cycles + 2);
    }
}
