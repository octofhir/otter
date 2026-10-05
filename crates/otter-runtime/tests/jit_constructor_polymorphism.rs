//! Stable compiled constructors shared by distinct actual closure families.
//!
//! # Contents
//! - Twenty-four prototype/layout owners sharing one captured constructor body.
//! - Interpreter parity and bounded compilation after source feedback settles.
//! - An isolated second drive with unchanged native generation and no subject
//!   interpreter dispatch.
//! - Actual descriptor/deletion/prototype mutation after all families settle.
//! - Current callee-prefix typed admission/commit edges and real moving child allocation.
//! - Split class new.targets and already selected super calls after static mutation.
//!
//! # Invariants
//! Normal source work admits both native tiers; no threshold or budget is
//! changed. Each constructor writes a captured family marker and a fresh child,
//! so canonical field preparation and ordinary property stores remain real.
//! Prototype identity, own keys and child values are checked on every receiver.
//! Complete event/dispatch counts distinguish native reuse from repeated
//! compilation or an interpreter fallback.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use otter_runtime::{
    JitArtifactFileName, JitDebugCompileOutcome, JitDebugEvent, JitDebugRequest, JitSelection,
    Runtime, RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError,
    RuntimeValue, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::native_abi::{CodeLifetimeState, NativeFrameKind};

const SETUP: &str = r#"
function familyFactory(kind) {
  function PolymorphicCell(value) {
    familyObserve(this, undefined, 0);
    this.value = value;
    this.kind = kind;
    this.child = {value: value};
    familyObserve(this, this.child, 1);
  }
  PolymorphicCell.prototype.marker = kind;
  return PolymorphicCell;
}
const constructorFamilies = [];
for (let kind = 0; kind < 24; kind++) constructorFamilies.push(familyFactory(kind));
function driveFamilies(rounds) {
  let sum = 0, bad = 0, last = -1;
  for (let i = 0; i < rounds; i++) {
    const Ctor = constructorFamilies[i % 24];
    const cell = new Ctor(i);
    if (cell.value !== i || cell.kind !== i % 24 || cell.child.value !== i
        || cell.marker !== i % 24 || Object.getPrototypeOf(cell) !== Ctor.prototype
        || Object.keys(cell).join(",") !== "value,kind,child") bad++;
    sum += cell.value + cell.kind;
    last = cell.kind;
  }
  return JSON.stringify([sum, bad, last]);
}
driveFamilies(12000);
"#;
const PROBE: &str = "driveFamilies(12000);";
const EXPECTED: &str = "[72132000,0,23]";
const MOVING_PROBE: &str = "familyStress(1); const movedFamilyResult = driveFamilies(96); familyStress(0); movedFamilyResult;";

#[derive(Debug)]
struct MovingObservation {
    phase: i32,
    receiver: u32,
    child: Option<u32>,
    minor: u64,
    active_code: u64,
}

fn installer(
    recording: Arc<AtomicBool>,
    fid: Arc<AtomicU32>,
    observations: Arc<Mutex<Vec<MovingObservation>>>,
) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        realm.install_native_global_call(
            "familyStress",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                |ctx: &mut RuntimeNativeCtx<'_>,
                 args: &[RuntimeValue],
                 _state: &[RuntimeValue]|
                 -> Result<RuntimeValue, RuntimeNativeError> {
                    let stride = args
                        .first()
                        .and_then(|v| v.as_number())
                        .ok_or_else(|| RuntimeNativeError::Error {
                            message: "missing family stress stride".into(),
                        })?
                        .as_f64() as u32;
                    ctx.interp_mut().gc_heap_mut().set_gc_stress(stride, true);
                    Ok(RuntimeValue::undefined())
                },
            )),
        )?;
        let recording = recording.clone();
        let fid = fid.clone();
        let observations = observations.clone();
        realm.install_native_global_call(
            "familyObserve",
            3,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]|
                      -> Result<RuntimeValue, RuntimeNativeError> {
                    if recording.load(Ordering::Relaxed) {
                        let phase = args
                            .get(2)
                            .and_then(|v| v.as_number())
                            .ok_or_else(|| RuntimeNativeError::Error {
                                message: "missing family phase".into(),
                            })?
                            .as_f64() as i32;
                        let receiver = args
                            .first()
                            .copied()
                            .and_then(RuntimeValue::as_object)
                            .ok_or_else(|| RuntimeNativeError::Error {
                                message: "missing current family receiver".into(),
                            })?
                            .offset();
                        let child = args
                            .get(1)
                            .copied()
                            .and_then(RuntimeValue::as_object)
                            .map(|v| v.offset());
                        let vm = ctx.interp_mut();
                        let minor = vm.gc_heap().gc_cycle_counts().0;
                        let active_code = vm
                            .jit_code_generation_snapshot()
                            .into_iter()
                            .find(|generation| {
                                generation.function_id == fid.load(Ordering::Relaxed)
                                    && generation.active_count > 0
                            })
                            .map_or(0, |generation| generation.code_object_id);
                        observations
                            .lock()
                            .map_err(|_| RuntimeNativeError::Error {
                                message: "family recorder poisoned".into(),
                            })?
                            .push(MovingObservation {
                                phase,
                                receiver,
                                child,
                                minor,
                                active_code,
                            });
                    }
                    Ok(RuntimeValue::undefined())
                },
            )),
        )
    })
}

// The stable native drive above settles all 24 source-sharing families. These
// mutations then exercise canonical preparation under actual inherited setters,
// descriptor retirement, deletion, chain replacement and a new family root.
const MUTATION_PROBE: &str = r#"
function verifyMutableFamilies() {
  let bad = 0, setterCalls = 0;
  for (let kind = 0; kind < 24; kind++) {
    const Ctor = constructorFamilies[kind], original = Ctor.prototype;
    const parent = Object.getPrototypeOf(original);
    Object.defineProperty(original, "value", {
      get: function () { return -1; },
      set: function (value) { setterCalls++; this.received = value; },
      configurable: true
    });
    let cell = new Ctor(kind + 100);
    if (cell.value !== -1 || cell.received !== kind + 100 || cell.kind !== kind
        || cell.child.value !== kind + 100 || Object.getPrototypeOf(cell) !== original
        || Object.keys(cell).join(",") !== "received,kind,child") bad++;
    delete original.value;
    cell = new Ctor(kind + 200);
    if (cell.value !== kind + 200 || cell.kind !== kind || cell.child.value !== kind + 200
        || Object.getPrototypeOf(cell) !== original
        || Object.keys(cell).join(",") !== "value,kind,child") bad++;
    Object.defineProperty(original, "kind", {value: -2, writable: false, configurable: true});
    cell = new Ctor(kind + 300);
    if (cell.value !== kind + 300 || cell.kind !== -2 || cell.child.value !== kind + 300
        || Object.getPrototypeOf(cell) !== original
        || Object.keys(cell).join(",") !== "value,child") bad++;
    delete original.kind;
    const inherited = {
      get value() { return -3; },
      set value(value) { setterCalls++; this.received = value; }
    };
    Object.setPrototypeOf(original, inherited);
    cell = new Ctor(kind + 400);
    if (cell.value !== -3 || cell.received !== kind + 400 || cell.kind !== kind
        || cell.child.value !== kind + 400 || Object.getPrototypeOf(cell) !== original
        || Object.keys(cell).join(",") !== "received,kind,child") bad++;
    Object.setPrototypeOf(original, parent);
    Ctor.prototype = {marker: kind};
    cell = new Ctor(kind + 500);
    if (cell.value !== kind + 500 || cell.kind !== kind || cell.child.value !== kind + 500
        || Object.getPrototypeOf(cell) !== Ctor.prototype || Ctor.prototype === original
        || cell.marker !== kind || Object.keys(cell).join(",") !== "value,kind,child") bad++;
  }
  return JSON.stringify([bad, setterCalls]);
}
verifyMutableFamilies();
"#;
const MUTATION_EXPECTED: &str = "[0,48]";

#[derive(Default)]
struct DispatchCounts {
    recording: bool,
    total: usize,
    by_function: BTreeMap<u32, usize>,
}

struct Tracer(Arc<Mutex<DispatchCounts>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut counts = self.0.lock().unwrap();
        if counts.recording {
            counts.total += 1;
            *counts.by_function.entry(event.function_id).or_default() += 1;
        }
    }
}

#[test]
fn alternating_constructor_families_keep_one_body_native_without_recompilation() {
    let recording = Arc::new(AtomicBool::new(false));
    let observed_fid = Arc::new(AtomicU32::new(u32::MAX));
    let observations = Arc::new(Mutex::new(Vec::<MovingObservation>::new()));
    let install = installer(
        recording.clone(),
        observed_fid.clone(),
        observations.clone(),
    );
    let mut oracle_runtime = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .extension_installer(install.clone())
        .build()
        .unwrap();
    let oracle = oracle_runtime
        .run_script(
            SourceInput::from_javascript(SETUP),
            "constructor-families-oracle.js",
        )
        .unwrap();
    assert_eq!(oracle.completion_string(), EXPECTED);
    let oracle_mutation = oracle_runtime
        .run_script(
            SourceInput::from_javascript(MUTATION_PROBE),
            "constructor-families-mutation-oracle.js",
        )
        .unwrap();
    assert_eq!(oracle_mutation.completion_string(), MUTATION_EXPECTED);

    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .extension_installer(install.clone())
            .build()
            .unwrap();
        let counts = Arc::new(Mutex::new(DispatchCounts::default()));
        runtime.set_tracer(Some(Box::new(Tracer(counts.clone()))));
        runtime
            .run_script(
                SourceInput::from_javascript("familyStress(0);"),
                "constructor-families-lab-enable.js",
            )
            .unwrap();
        let warm = runtime
            .run_script(
                SourceInput::from_javascript(SETUP),
                "constructor-families.js",
            )
            .unwrap();
        assert_eq!(warm.completion_string(), EXPECTED, "{selection:?}");
        let report = warm
            .jit_debug_report()
            .expect("complete native compilation events");
        assert!(!report.truncated());
        assert_eq!(report.dropped_events(), 0);
        let function_ids: BTreeSet<_> = report
            .events()
            .iter()
            .filter_map(|event| match event {
                JitDebugEvent::CompilePrepared {
                    function_id,
                    function_name,
                    ..
                } if function_name == "PolymorphicCell" => Some(*function_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            function_ids.len(),
            1,
            "all actual closures share one source body"
        );
        let fid = *function_ids.first().unwrap();
        let compiled = report.events().iter().filter(|event| matches!(event,
            JitDebugEvent::CompileFinished { function_id, outcome: JitDebugCompileOutcome::Compiled { .. }, .. }
                if *function_id == fid
        )).count();
        assert!(
            (1..=4).contains(&compiled),
            "{selection:?}: settled receiver families must not create an eviction storm: {compiled}"
        );
        let tier = match selection {
            JitSelection::Template => NativeFrameKind::Baseline,
            JitSelection::ProductionTiered => NativeFrameKind::Optimizing,
            JitSelection::InterpreterOnly => unreachable!(),
        };
        let current: Vec<_> = runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .filter(|generation| {
                generation.function_id == fid
                    && generation.tier == tier
                    && generation.lifecycle == CodeLifetimeState::Installed
                    && generation.linked
                    && generation.call_entry_offset.is_some()
            })
            .collect();
        assert_eq!(
            current.len(),
            1,
            "{selection:?}: exact own native constructor"
        );
        let generation = &current[0];
        let artifacts = warm
            .jit_artifacts()
            .expect("exact own callee entry artifact");
        assert!(!artifacts.truncated());
        let own = artifacts
            .bundles()
            .iter()
            .find(|bundle| bundle.manifest().code_object_id() == generation.code_object_id)
            .expect("current own constructor bundle");
        let relocations: serde_json::Value = serde_json::from_slice(
            own.file(JitArtifactFileName::Relocations)
                .unwrap()
                .contents(),
        )
        .unwrap();
        for name in ["constructor_receiver_probe", "constructor_receiver_commit"] {
            assert!(
                relocations["relocations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["target"]["name"] == name),
                "own generated callee prefix must contain {name}"
            );
        }
        let before = runtime.execution_stats();
        counts.lock().unwrap().recording = true;
        let probe = runtime
            .run_script(
                SourceInput::from_javascript(PROBE),
                "constructor-families-probe.js",
            )
            .unwrap();
        assert_eq!(probe.completion_string(), EXPECTED, "{selection:?}");
        let stats = runtime.execution_stats();
        assert!(
            stats.jit_receiver_alloc_generated > before.jit_receiver_alloc_generated,
            "actual current callee prefix must publish receiver LAB fits for alternating families"
        );
        let observed = counts.lock().unwrap();
        assert!(
            observed.total > 0,
            "the complete isolated probe observed its script dispatch"
        );
        assert_eq!(
            observed.by_function.get(&fid).copied().unwrap_or(0),
            0,
            "{selection:?}: every constructor body in the second drive stays native"
        );
        let report = probe.jit_debug_report().unwrap();
        assert!(!report.truncated());
        assert_eq!(report.dropped_events(), 0);
        assert!(
            !report.events().iter().any(|event| matches!(event,
                JitDebugEvent::CompileFinished { function_id, .. } if *function_id == fid
            )),
            "{selection:?}: settled family observations cannot recompile the constructor"
        );
        let after = runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .find(|after| after.code_object_id == generation.code_object_id)
            .unwrap();
        assert_eq!(after.lifecycle, CodeLifetimeState::Installed);
        assert!(after.linked);
        assert_eq!(after.generated_deopts, generation.generated_deopts);
        assert_eq!(after.active_count, 0);
        drop(observed);
        // The same installed native constructor now runs with the production
        // disabled LAB. The source child literal performs the actual moving
        // collection while receiver/child homes and original ticket remain live.
        observed_fid.store(fid, Ordering::Relaxed);
        observations.lock().unwrap().clear();
        recording.store(true, Ordering::Relaxed);
        let before_gc = runtime.execution_stats();
        let moved = runtime
            .run_script(
                SourceInput::from_javascript(MOVING_PROBE),
                "constructor-families-moving.js",
            )
            .unwrap();
        recording.store(false, Ordering::Relaxed);
        assert_eq!(moved.completion_string(), "[5664,0,23]");
        let after_gc = runtime.execution_stats();
        assert_eq!(
            after_gc.jit_receiver_alloc_generated, before_gc.jit_receiver_alloc_generated,
            "disabled current LAB must miss before publication"
        );
        let recorded = observations.lock().unwrap();
        assert_eq!(
            recorded.len(),
            192,
            "all 96 actual constructors observed before/after child allocation"
        );
        for pair in recorded.chunks_exact(2) {
            assert_eq!((pair[0].phase, pair[1].phase), (0, 1));
            assert!(pair[0].child.is_none());
            assert!(pair[1].child.is_some());
            assert_eq!(pair[0].active_code, generation.code_object_id);
            assert_eq!(
                pair[1].active_code, generation.code_object_id,
                "the exact own native body is active through real collection"
            );
            assert!(
                pair[1].minor > pair[0].minor,
                "actual child allocation collects"
            );
            assert_ne!(
                pair[0].receiver, pair[1].receiver,
                "the exact original receiver moved in the native interval"
            );
        }
        drop(recorded);
        let after = runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .find(|after| after.code_object_id == generation.code_object_id)
            .unwrap();
        assert_eq!(after.lifecycle, CodeLifetimeState::Installed);
        assert!(after.linked);
        assert_eq!(after.generated_deopts, generation.generated_deopts);
        assert_eq!(after.active_count, 0);
        let observed = counts.lock().unwrap();
        assert_eq!(
            observed.by_function.get(&fid).copied().unwrap_or(0),
            0,
            "moving callee stays native"
        );
        drop(observed);
        let mutation = runtime
            .run_script(
                SourceInput::from_javascript(MUTATION_PROBE),
                "constructor-families-mutations.js",
            )
            .unwrap();
        assert_eq!(
            mutation.completion_string(),
            MUTATION_EXPECTED,
            "{selection:?}: invalidated preparation must preserve actual inherited stores and family identity"
        );
    }
}

// The base receiver is selected by the actual new.target family record. Neither
// an unrelated class body nor its current static superclass determines which
// already selected bytecode body receives that receiver.
const OWNER_RECORD_SETUP: &str = r#"
function ActualRecordBase(value) {
  familyObserve(this, undefined, 0);
  this.value = value;
  this.child = {value: value};
  familyObserve(this, this.child, 1);
}
class UnrelatedNewTarget {}
function OtherActualRecordBase() { this.other = 1; }
function driveOwnerSplit(rounds) {
  let sum = 0, bad = 0, last = -1;
  for (let i = 0; i < rounds; i++) {
    const cell = Reflect.construct(ActualRecordBase, [i], UnrelatedNewTarget);
    if (cell.value !== i || cell.child.value !== i
        || Object.getPrototypeOf(cell) !== UnrelatedNewTarget.prototype
        || Object.keys(cell).join(",") !== "value,child") bad++;
    sum += cell.value;
    last = cell.child.value;
  }
  return [sum, bad, last];
}
class ActualRecordSuperBase {
  constructor(value) {
    familyObserve(this, undefined, 0);
    this.value = value;
    this.child = {value: value};
    familyObserve(this, this.child, 1);
  }
}
function replaceSelectedSuper(value) {
  Object.setPrototypeOf(ChangingNewTarget, UnrelatedNewTarget);
  return value;
}
class ChangingNewTarget extends ActualRecordSuperBase {
  constructor(value) {
    // GetSuperConstructor precedes argument evaluation. The original base
    // still executes after this argument changes the new.target static chain.
    super(replaceSelectedSuper(value));
    this.derived = value + 1;
  }
}
function driveOwnerSuper(rounds) {
  let sum = 0, bad = 0, last = -1;
  for (let i = 0; i < rounds; i++) {
    Object.setPrototypeOf(ChangingNewTarget, ActualRecordSuperBase);
    const cell = new ChangingNewTarget(i);
    if (cell.value !== i || cell.child.value !== i || cell.derived !== i + 1
        || Object.getPrototypeOf(cell) !== ChangingNewTarget.prototype
        || Object.getPrototypeOf(ChangingNewTarget) !== UnrelatedNewTarget
        || Object.keys(cell).join(",") !== "value,child,derived") bad++;
    sum += cell.value + cell.derived;
    last = cell.child.value;
  }
  return [sum, bad, last];
}
JSON.stringify([driveOwnerSplit(6000), driveOwnerSuper(6000)]);
"#;
const OWNER_RECORD_WARM_EXPECTED: &str = "[[17997000,0,5999],[36000000,0,5999]]";
const OWNER_RECORD_WRONG_BASE: &str = r#"
Reflect.construct(OtherActualRecordBase, [], UnrelatedNewTarget);
const ownerAfterWrongBase = Reflect.construct(ActualRecordBase, [37], UnrelatedNewTarget);
JSON.stringify([ownerAfterWrongBase.value, ownerAfterWrongBase.child.value,
  Object.getPrototypeOf(ownerAfterWrongBase) === UnrelatedNewTarget.prototype,
  Object.keys(ownerAfterWrongBase).join(",")]);
"#;
const OWNER_RECORD_INVALID_INSTANCE_CHAIN: &str = r#"
let ownerSetterCalls = 0;
Object.setPrototypeOf(UnrelatedNewTarget.prototype, {
  get value() { return -1; },
  set value(value) { ownerSetterCalls++; this.received = value; }
});
const ownerAfterInstanceChange = Reflect.construct(ActualRecordBase, [77], UnrelatedNewTarget);
JSON.stringify([ownerAfterInstanceChange.value, ownerAfterInstanceChange.received,
  ownerAfterInstanceChange.child.value, ownerSetterCalls,
  Object.getPrototypeOf(ownerAfterInstanceChange) === UnrelatedNewTarget.prototype,
  Object.keys(ownerAfterInstanceChange).join(",")]);
"#;

#[test]
fn actual_owner_records_fit_split_classes_and_super_selected_before_argument_mutation() {
    let recording = Arc::new(AtomicBool::new(false));
    let observed_fid = Arc::new(AtomicU32::new(u32::MAX));
    let observations = Arc::new(Mutex::new(Vec::<MovingObservation>::new()));
    let install = installer(
        recording.clone(),
        observed_fid.clone(),
        observations.clone(),
    );
    let mut oracle = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .extension_installer(install.clone())
        .build()
        .unwrap();
    let warm = oracle
        .run_script(
            SourceInput::from_javascript(OWNER_RECORD_SETUP),
            "owner-record-oracle.js",
        )
        .unwrap();
    assert_eq!(warm.completion_string(), OWNER_RECORD_WARM_EXPECTED);
    for (program, expected) in [
        (OWNER_RECORD_WRONG_BASE, "[37,37,true,\"value,child\"]"),
        (
            OWNER_RECORD_INVALID_INSTANCE_CHAIN,
            "[-1,77,77,1,true,\"received,child\"]",
        ),
    ] {
        let result = oracle
            .run_script(
                SourceInput::from_javascript(program),
                "owner-record-mutation-oracle.js",
            )
            .unwrap();
        assert_eq!(result.completion_string(), expected);
    }

    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .extension_installer(install.clone())
            .build()
            .unwrap();
        runtime
            .run_script(
                SourceInput::from_javascript("familyStress(0);"),
                "owner-record-lab-enable.js",
            )
            .unwrap();
        let warm = runtime
            .run_script(
                SourceInput::from_javascript(OWNER_RECORD_SETUP),
                "owner-record-native.js",
            )
            .unwrap();
        assert_eq!(warm.completion_string(), OWNER_RECORD_WARM_EXPECTED);
        let report = warm.jit_debug_report().unwrap();
        assert!(!report.truncated());
        assert_eq!(report.dropped_events(), 0);
        let artifacts = warm.jit_artifacts().unwrap();
        assert!(!artifacts.truncated());
        let tier = match selection {
            JitSelection::Template => NativeFrameKind::Baseline,
            JitSelection::ProductionTiered => NativeFrameKind::Optimizing,
            JitSelection::InterpreterOnly => unreachable!(),
        };
        let counts = Arc::new(Mutex::new(DispatchCounts::default()));
        runtime.set_tracer(Some(Box::new(Tracer(counts.clone()))));
        for (name, program, expected) in [
            (
                "ActualRecordBase",
                "JSON.stringify(driveOwnerSplit(128));",
                "[8128,0,127]",
            ),
            (
                "ActualRecordSuperBase",
                "JSON.stringify(driveOwnerSuper(128));",
                "[16384,0,127]",
            ),
        ] {
            let fids: BTreeSet<_> = report
                .events()
                .iter()
                .filter_map(|event| match event {
                    JitDebugEvent::CompilePrepared {
                        function_id,
                        function_name,
                        ..
                    } if function_name == name => Some(*function_id),
                    _ => None,
                })
                .collect();
            assert_eq!(fids.len(), 1, "one actual source body for {name}");
            let fid = *fids.first().unwrap();
            let generations: Vec<_> = runtime
                .jit_code_generation_snapshot()
                .into_iter()
                .filter(|generation| {
                    generation.function_id == fid
                        && generation.tier == tier
                        && generation.lifecycle == CodeLifetimeState::Installed
                        && generation.linked
                        && generation.call_entry_offset.is_some()
                })
                .collect();
            assert_eq!(
                generations.len(),
                1,
                "{selection:?}: own current {name} body"
            );
            let generation = &generations[0];
            let own = artifacts
                .bundles()
                .iter()
                .find(|bundle| bundle.manifest().code_object_id() == generation.code_object_id)
                .unwrap();
            let relocations: serde_json::Value = serde_json::from_slice(
                own.file(JitArtifactFileName::Relocations)
                    .unwrap()
                    .contents(),
            )
            .unwrap();
            for leaf in ["constructor_receiver_probe", "constructor_receiver_commit"] {
                assert!(
                    relocations["relocations"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|row| row["target"]["name"] == leaf),
                    "own prefix contains {leaf}"
                );
            }
            observed_fid.store(fid, Ordering::Relaxed);
            observations.lock().unwrap().clear();
            *counts.lock().unwrap() = DispatchCounts {
                recording: true,
                ..Default::default()
            };
            recording.store(true, Ordering::Relaxed);
            let before = runtime.execution_stats();
            let result = runtime
                .run_script(
                    SourceInput::from_javascript(program),
                    "owner-record-isolated-native.js",
                )
                .unwrap();
            recording.store(false, Ordering::Relaxed);
            counts.lock().unwrap().recording = false;
            assert_eq!(
                result.completion_string(),
                expected,
                "{selection:?}: {name}"
            );
            assert!(
                runtime.execution_stats().jit_receiver_alloc_generated
                    > before.jit_receiver_alloc_generated,
                "{selection:?}: {name} actual owner must admit generated receiver fits"
            );
            let observed = observations.lock().unwrap();
            assert_eq!(observed.len(), 256, "all 128 receiver/body observations");
            for pair in observed.chunks_exact(2) {
                assert_eq!((pair[0].phase, pair[1].phase), (0, 1));
                assert!(pair[0].child.is_none() && pair[1].child.is_some());
                assert_eq!(pair[0].active_code, generation.code_object_id);
                assert_eq!(pair[1].active_code, generation.code_object_id);
            }
            drop(observed);
            let counts = counts.lock().unwrap();
            assert!(counts.total > 0);
            assert_eq!(
                counts.by_function.get(&fid).copied().unwrap_or(0),
                0,
                "{selection:?}: exact {name} constructor stays native"
            );
            drop(counts);
            let report = result.jit_debug_report().unwrap();
            assert!(!report.truncated());
            assert!(
                !report.events().iter().any(|event| matches!(event,
                    JitDebugEvent::CompileFinished { function_id, .. } if *function_id == fid
                )),
                "isolated owner admission retains the current generation"
            );
            let after = runtime
                .jit_code_generation_snapshot()
                .into_iter()
                .find(|after| after.code_object_id == generation.code_object_id)
                .unwrap();
            assert_eq!(after.lifecycle, CodeLifetimeState::Installed);
            assert!(after.linked);
            assert_eq!(after.generated_deopts, generation.generated_deopts);
            assert_eq!(after.active_count, 0);
        }
        for (program, expected) in [
            (OWNER_RECORD_WRONG_BASE, "[37,37,true,\"value,child\"]"),
            (
                OWNER_RECORD_INVALID_INSTANCE_CHAIN,
                "[-1,77,77,1,true,\"received,child\"]",
            ),
        ] {
            let before = runtime.execution_stats();
            let result = runtime
                .run_script(
                    SourceInput::from_javascript(program),
                    "owner-record-pre-effect-miss.js",
                )
                .unwrap();
            assert_eq!(result.completion_string(), expected);
            assert_eq!(
                runtime.execution_stats().jit_receiver_alloc_generated,
                before.jit_receiver_alloc_generated,
                "wrong current base or invalidated instance-chain proof must prepare canonically"
            );
        }
    }
}
