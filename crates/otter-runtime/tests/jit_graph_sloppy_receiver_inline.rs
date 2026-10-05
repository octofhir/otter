//! Native explicit Object receiver splices with real source and moving roots.
//!
//! # Contents
//! - Sloppy constructor-style `.call` chains under the production tier policy.
//! - Own accepted Graph body, nested source recipes and isolated native entry.
//! - Evaluated intrinsic retention, one eager recovery and collecting arguments.
//! - Strict, primitive, nullish and lexical bindings on their canonical paths.
//!
//! # Invariants
//! Only actual admitted source and the real collector execute. Observer data is
//! owned; no callback assertion or moving handle escapes its scope. The loaded
//! callable and all actuals precede the inline guards. Budgets, counters, call
//! ABI and the canonical receiver-conversion owner remain unchanged.
//!
//! # See also
//! - `graph::builder` owns the SSA Object receiver fact and body splice.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use otter_bytecode::Op;
use otter_runtime::{
    JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError,
    RuntimeValue, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{CodeLifetimeState, NativeFrameKind},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

const SETUP: &str = r#"
receiverStress(0);
globalThis.receiverReplacementCount = 0;
function receiverBase(child) {
  this.child = child;
  this.base = 17;
  receiverObserve(this, child);
}
function receiverMiddle(child) {
  receiverBase.call(this, child);
  this.middle = 19;
}
function receiverRoot(child) {
  receiverMiddle.call(this, receiverEvaluate(child, this));
  this.root = 23;
  return this;
}
function receiverMake(index) { return new receiverRoot(receiverWarmChild); }
function receiverMutation() {
  receiverMiddle.call = function receiverReplacement(receiver, child) {
    receiverReplacementCount++;
    receiver.changed = child;
  };
}
function receiverStrict(value) { 'use strict'; return receiverBinding.call(this, value); }
function receiverBinding(value) { return [typeof this, this === globalThis, this === value]; }
const receiverLexical = (function () { const arrow = () => this; return arrow; }).call({lexical: 37});
globalThis.receiverWarmChild = {marker: 731};
new Array(5000).fill(0).map(receiverMake).length;
"#;
const HIT: &str = r#"
(function receiverHitProbe() {
  const result = new receiverRoot(receiverWarmChild);
  return JSON.stringify([result.child === receiverWarmChild, result.base, result.middle, result.root]);
})()
"#;
const MOVING: &str = r#"
(function receiverMovingProbe() {
  receiverCollect();
  const child = {marker: 731};
  const alias = child;
  const result = new receiverRoot(child);
  const later = {};
  receiverMiddle.call(later, child);
  delete receiverMiddle.call;
  return JSON.stringify([result.child === alias, result.child.marker, result.base, result.middle, result.root,
    later.changed === alias, receiverReplacementCount]);
})()
"#;
const RECOVER: &str = r#"
(function receiverRecoveryProbe() {
  receiverCollect();
  const child = {marker: 731};
  const alias = child;
  Object.defineProperty(receiverBase, 'call', {
    configurable: true,
    get() {
      receiverReplacementCount++;
      return function receiverRecovered(receiver, currentChild) {
        receiver.child = currentChild;
        receiver.base = 41;
      };
    }
  });
  const result = new receiverRoot(child);
  delete receiverBase.call;
  return JSON.stringify([result.child === alias, result.child.marker, result.base, result.middle, result.root,
    receiverReplacementCount]);
})()
"#;
const DOMAINS: &str = r#"
(function receiverDomainProbe() {
  const number = receiverStrict.call(7, 7);
  const text = receiverStrict.call('q', 'q');
  const boolean = receiverStrict.call(true, true);
  const symbol = Symbol('receiver');
  const boxedSymbol = receiverStrict.call(symbol, symbol);
  const nullish = receiverStrict.call(null, globalThis);
  const undefinedThis = receiverStrict.call(undefined, globalThis);
  const object = {marker: 731};
  const proxy = new Proxy(object, {});
  function callable() {}
  const proxyResult = receiverBinding.call(proxy, proxy);
  const callableResult = receiverBinding.call(callable, callable);
  const strictIdentity = (function strictIdentity() { 'use strict'; return this; }).call(7) === 7;
  const arrowIdentity = receiverLexical.call({lexical: 99}).lexical === 37;
  const boxedRoot = receiverRoot.call(7, receiverWarmChild);
  const nullRoot = receiverRoot.call(null, receiverWarmChild);
  return JSON.stringify([number, text, boolean, boxedSymbol, nullish, undefinedThis, proxyResult, callableResult,
    strictIdentity, arrowIdentity, typeof boxedRoot === 'object' && boxedRoot.child === receiverWarmChild,
    nullRoot === globalThis && nullRoot.child === receiverWarmChild]);
})()
"#;

#[derive(Default)]
struct Trace {
    recording: bool,
    counts: BTreeMap<u32, usize>,
    calls: BTreeMap<u32, BTreeSet<(u32, bool)>>,
}
struct Tracer(Arc<Mutex<Trace>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self.0.lock().unwrap();
        if matches!(event.op, Op::CallWithThis | Op::CallMethodValue) {
            trace
                .calls
                .entry(event.function_id)
                .or_default()
                .insert((event.byte_pc, event.op == Op::CallMethodValue));
        }
        if trace.recording {
            *trace.counts.entry(event.function_id).or_default() += 1;
        }
    }
}
#[derive(Debug)]
struct Motion {
    receiver_before: u32,
    receiver_after: u32,
    child_before: u32,
    child_after: u32,
    marker: f64,
    minor_before: u64,
    minor_after: u64,
    updates_before: u64,
    updates_after: u64,
}
#[derive(Debug)]
struct Observed {
    codes: Vec<u64>,
    source: String,
    receiver: u32,
    child: u32,
}
#[derive(Default)]
struct Records {
    evaluations: usize,
    motions: Vec<Result<Motion, String>>,
    bodies: Vec<Result<Observed, String>>,
}
fn installer(
    recording: Arc<AtomicBool>,
    moving: Arc<AtomicBool>,
    mutate: Arc<AtomicBool>,
    records: Arc<Mutex<Records>>,
) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        realm.install_native_global_call(
            "receiverStress",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _state: &[RuntimeValue]| {
                    let stride = args
                        .first()
                        .and_then(|value| value.as_number())
                        .ok_or(RuntimeNativeError::InvalidOperand)?
                        .as_f64() as u32;
                    ctx.interp_mut().gc_heap_mut().set_gc_stress(stride, true);
                    Ok(RuntimeValue::undefined())
                },
            )),
        )?;
        realm.install_native_global_call(
            "receiverOffset",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                |_ctx: &mut RuntimeNativeCtx<'_>,
                 args: &[RuntimeValue],
                 _state: &[RuntimeValue]| {
                    let object = args
                        .first()
                        .and_then(|value| value.as_object())
                        .ok_or(RuntimeNativeError::InvalidOperand)?;
                    Ok(RuntimeValue::number_i32(object.offset() as i32))
                },
            )),
        )?;
        realm.install_native_global_call(
            "receiverCollect",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                |ctx: &mut RuntimeNativeCtx<'_>,
                 _args: &[RuntimeValue],
                 _state: &[RuntimeValue]| {
                    ctx.interp_mut()
                        .force_gc()
                        .map_err(RuntimeNativeError::from)?;
                    Ok(RuntimeValue::undefined())
                },
            )),
        )?;
        let evaluations = records.clone();
        let armed = moving.clone();
        let mutation = mutate.clone();
        let active = recording.clone();
        realm.install_native_global_call(
            "receiverEvaluate",
            2,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    if active.load(Ordering::Relaxed) {
                        evaluations
                            .lock()
                            .map_err(|_| RuntimeNativeError::InvalidOperand)?
                            .evaluations += 1;
                    }
                    if !armed.swap(false, Ordering::Relaxed) {
                        return Ok(args
                            .first()
                            .copied()
                            .unwrap_or_else(RuntimeValue::undefined));
                    }
                    let before = ctx.interp_mut().gc_stats_snapshot();
                    let change = mutation.swap(false, Ordering::Relaxed);
                    let observed = ctx.scope(|mut scope| -> Result<_, RuntimeNativeError> {
                        let child = scope.argument(args, 0);
                        let receiver = scope.argument(args, 1);
                        let offset = scope
                            .global("receiverOffset")
                            .ok_or(RuntimeNativeError::InvalidOperand)?;
                        let collect = scope
                            .global("receiverCollect")
                            .ok_or(RuntimeNativeError::InvalidOperand)?;
                        let mutation = scope
                            .global("receiverMutation")
                            .ok_or(RuntimeNativeError::InvalidOperand)?;
                        let undefined = scope.undefined();
                        let old = scope.call(offset, undefined, &[receiver])?;
                        let receiver_before = scope.number_value(old)? as i32 as u32;
                        let old = scope.call(offset, undefined, &[child])?;
                        let child_before = scope.number_value(old)? as i32 as u32;
                        scope.call(collect, undefined, &[])?;
                        if change {
                            scope.call(mutation, undefined, &[])?;
                        }
                        let new = scope.call(offset, undefined, &[receiver])?;
                        let receiver_after = scope.number_value(new)? as i32 as u32;
                        let new = scope.call(offset, undefined, &[child])?;
                        let child_after = scope.number_value(new)? as i32 as u32;
                        let marker = scope.get(child, "marker")?;
                        let marker = scope.number_value(marker)?;
                        let motion = Motion {
                            receiver_before,
                            receiver_after,
                            child_before,
                            child_after,
                            marker,
                            minor_before: before.minor_gc_cycles,
                            minor_after: 0,
                            updates_before: before.minor_slot_updates,
                            updates_after: 0,
                        };
                        Ok((motion, scope.finish(child)))
                    });
                    match observed {
                        Ok((mut motion, child)) => {
                            let after = ctx.interp_mut().gc_stats_snapshot();
                            motion.minor_after = after.minor_gc_cycles;
                            motion.updates_after = after.minor_slot_updates;
                            evaluations
                                .lock()
                                .map_err(|_| RuntimeNativeError::InvalidOperand)?
                                .motions
                                .push(Ok(motion));
                            Ok(child)
                        }
                        Err(error) => {
                            evaluations
                                .lock()
                                .map_err(|_| RuntimeNativeError::InvalidOperand)?
                                .motions
                                .push(Err(error.to_string()));
                            Err(error)
                        }
                    }
                },
            )),
        )?;
        let observed = records.clone();
        let active = recording.clone();
        realm.install_native_global_call(
            "receiverObserve",
            2,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    if active.load(Ordering::Relaxed) {
                        let observation = (|| -> Result<Observed, String> {
                            let receiver = args
                                .first()
                                .and_then(|value| value.as_object())
                                .ok_or("missing bound receiver")?
                                .offset();
                            let child = args
                                .get(1)
                                .and_then(|value| value.as_object())
                                .ok_or("missing current child")?
                                .offset();
                            let source = ctx
                                .execution_context()
                                .map(|context| ctx.capture_call_sites_json(context, 0, usize::MAX))
                                .ok_or("missing actual source context")?;
                            let codes = ctx
                                .interp_mut()
                                .jit_code_generation_snapshot()
                                .into_iter()
                                .filter(|generation| generation.active_count > 0)
                                .map(|generation| generation.code_object_id)
                                .collect();
                            Ok(Observed {
                                codes,
                                source,
                                receiver,
                                child,
                            })
                        })();
                        observed
                            .lock()
                            .map_err(|_| RuntimeNativeError::InvalidOperand)?
                            .bodies
                            .push(observation);
                    }
                    Ok(RuntimeValue::undefined())
                },
            )),
        )
    })
}
fn run(runtime: &mut Runtime, source: &str, module: &str) -> otter_runtime::ExecutionResult {
    runtime
        .run_script(SourceInput::from_javascript(source), module)
        .expect("receiver source completes")
}
fn current(runtime: &Runtime, fid: u32) -> JitCodeGenerationSnapshot {
    let generations: Vec<_> = runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .filter(|g| {
            g.function_id == fid
                && g.tier == NativeFrameKind::Optimizing
                && g.lifecycle == CodeLifetimeState::Installed
                && g.linked
                && g.current_entry
        })
        .collect();
    assert_eq!(
        generations.len(),
        1,
        "own current Graph entry: {generations:?}"
    );
    generations.into_iter().next().unwrap()
}
fn clear(trace: &Arc<Mutex<Trace>>, records: &Arc<Mutex<Records>>) {
    let mut trace = trace.lock().unwrap();
    trace.recording = true;
    trace.counts.clear();
    *records.lock().unwrap() = Records::default();
}
fn motion(records: &Records) -> &Motion {
    assert_eq!(records.evaluations, 1, "argument evaluation exactly once");
    assert_eq!(records.motions.len(), 1);
    let motion = records.motions[0].as_ref().unwrap();
    assert_ne!(motion.receiver_before, motion.receiver_after);
    assert_ne!(motion.child_before, motion.child_after);
    assert_eq!(motion.marker, 731.0);
    assert!(motion.minor_after > motion.minor_before);
    assert!(motion.updates_after > motion.updates_before);
    motion
}
fn native(
    runtime: &Runtime,
    trace: &Arc<Mutex<Trace>>,
    generation: &JitCodeGenerationSnapshot,
    subjects: &BTreeSet<u32>,
    result: &otter_runtime::ExecutionResult,
) {
    let counts = &trace.lock().unwrap().counts;
    assert!(
        counts.values().sum::<usize>() > 0,
        "actual outer probe dispatch observed"
    );
    assert!(
        subjects
            .iter()
            .all(|fid| counts.get(fid).copied().unwrap_or(0) == 0),
        "subject bodies stay native: {counts:?}"
    );
    let after = current(runtime, generation.function_id);
    assert_eq!(after.code_object_id, generation.code_object_id);
    assert_eq!(after.generated_deopts, generation.generated_deopts);
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
        "native phase exits: {:?}",
        report.events()
    );
}
#[test]
fn actual_sloppy_object_splices_keep_loaded_intrinsics_source_recipes_and_moving_aliases() {
    let recording = Arc::new(AtomicBool::new(false));
    let moving = Arc::new(AtomicBool::new(false));
    let mutate = Arc::new(AtomicBool::new(false));
    let records = Arc::new(Mutex::new(Records::default()));
    let install = installer(
        recording.clone(),
        moving.clone(),
        mutate.clone(),
        records.clone(),
    );
    let mut oracle = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .extension_installer(install.clone())
        .build()
        .unwrap();
    assert_eq!(
        run(&mut oracle, SETUP, "receiver:oracle-setup").completion_string(),
        "5000"
    );
    let expected_hit = run(&mut oracle, HIT, "receiver:oracle-hit")
        .completion_string()
        .to_owned();
    moving.store(true, Ordering::Relaxed);
    mutate.store(true, Ordering::Relaxed);
    let expected_moving = run(&mut oracle, MOVING, "receiver:oracle-moving")
        .completion_string()
        .to_owned();
    moving.store(true, Ordering::Relaxed);
    let expected_recovery = run(&mut oracle, RECOVER, "receiver:oracle-recovery")
        .completion_string()
        .to_owned();
    let expected_domains = run(&mut oracle, DOMAINS, "receiver:oracle-domains")
        .completion_string()
        .to_owned();
    assert_eq!(
        expected_domains,
        "[[\"object\",false,false],[\"object\",false,false],[\"object\",false,false],[\"object\",false,false],[\"object\",true,true],[\"object\",true,true],[\"object\",false,true],[\"function\",false,true],true,true,true,true]"
    );
    assert_eq!(expected_hit, "[true,17,19,23]");
    assert_eq!(expected_moving, "[true,731,17,19,23,true,1]");
    assert_eq!(expected_recovery, "[true,731,41,19,23,2]");
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(install)
        .build()
        .unwrap();
    let trace = Arc::new(Mutex::new(Trace::default()));
    runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    let warm = run(&mut runtime, SETUP, "receiver-inline-setup.js");
    assert_eq!(warm.completion_string(), "5000");
    let report = warm.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    let names: BTreeMap<_, _> = report
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::CompilePrepared {
                function_id,
                function_name,
                ..
            } => Some((function_name.clone(), *function_id)),
            _ => None,
        })
        .collect();
    let root = names["receiverRoot"];
    let middle = names["receiverMiddle"];
    let base = names["receiverBase"];
    let subjects = BTreeSet::from([root, middle, base]);
    let generation = current(&runtime, root);
    let splices: Vec<_> = report
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::InlineLowered {
                code_object_id,
                tier: JitDebugTier::Optimizing,
                parent_function_id,
                callee_function_id,
                depth,
                ..
            } if *code_object_id == generation.code_object_id => {
                Some((*parent_function_id, *callee_function_id, *depth))
            }
            _ => None,
        })
        .collect();
    let preparations: Vec<_> = report
        .events()
        .iter()
        .filter(|event| matches!(event, JitDebugEvent::InlineCandidate { caller_function_id, .. } if subjects.contains(caller_function_id)))
        .collect();
    let artifacts = warm.jit_artifacts().unwrap();
    assert!(!artifacts.truncated());
    let subject_ir: Vec<_> = artifacts
        .bundles()
        .iter()
        .filter(|bundle| subjects.contains(&bundle.manifest().function_id()))
        .filter_map(|bundle| {
            bundle.file(JitArtifactFileName::OptimizedIr).map(|file| {
                (
                    bundle.manifest().function_id(),
                    bundle.manifest().code_object_id(),
                    std::str::from_utf8(file.contents()).unwrap(),
                )
            })
        })
        .collect();
    assert!(
        splices.contains(&(root, middle, 1)),
        "actual explicit-receiver root splice: {splices:?}; preparations: {preparations:#?}; subject IR: {subject_ir:#?}"
    );
    assert!(
        splices.contains(&(middle, base, 2)),
        "nested inherited receiver splice: {splices:?}"
    );
    let own = artifacts
        .bundles()
        .iter()
        .find(|bundle| bundle.manifest().code_object_id() == generation.code_object_id)
        .unwrap();
    let ir = std::str::from_utf8(
        own.file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert!(ir.contains("CheckFunctionPrototypeCall"));
    let map: serde_json::Value =
        serde_json::from_slice(own.file(JitArtifactFileName::CodeMap).unwrap().contents()).unwrap();
    let call_sites = trace.lock().unwrap().calls.clone();
    for (fid, method) in [(root, false), (middle, true)] {
        let sites: Vec<_> = call_sites[&fid]
            .iter()
            .filter(|site| site.1 == method)
            .collect();
        assert_eq!(sites.len(), 1, "exact original call spelling and source PC");
        let site = sites[0];
        let regions: Vec<_> = map["regions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|region| region["functionId"] == fid && region["bytePc"] == site.0)
            .collect();
        assert!(
            regions.iter().any(|region| region["operation"]
                .as_str()
                .is_some_and(|op| op.contains("CheckFunctionPrototypeCall"))
                && region["startOffset"].as_u64().unwrap() < region["endOffset"].as_u64().unwrap()),
            "physical intrinsic guard at the own source call: {regions:?}"
        );
        assert!(
            !regions.iter().any(|region| region["operation"]
                .as_str()
                .is_some_and(|op| op.contains("CallJs"))),
            "JS body call replaced by the actual splice"
        );
    }
    let deopt: serde_json::Value =
        serde_json::from_slice(own.file(JitArtifactFileName::Deopt).unwrap().contents()).unwrap();
    assert!(
        deopt["frameStates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|state| state["frames"]
                .as_array()
                .is_some_and(|frames| frames.len() == 3
                    && frames[0]["functionId"] == root
                    && frames[1]["functionId"] == middle
                    && frames[2]["functionId"] == base)),
        "own three-source eager recipes: {deopt}"
    );

    recording.store(true, Ordering::Relaxed);
    clear(&trace, &records);
    let hit = run(&mut runtime, HIT, "receiver:native-hit");
    assert_eq!(hit.completion_string(), expected_hit);
    native(&runtime, &trace, &generation, &subjects, &hit);
    {
        let records = records.lock().unwrap();
        assert_eq!(records.evaluations, 1);
        assert_eq!(records.bodies.len(), 1);
        let body = records.bodies[0].as_ref().unwrap();
        assert!(body.codes.contains(&generation.code_object_id));
        let source: serde_json::Value = serde_json::from_str(&body.source).unwrap();
        for name in ["receiverRoot", "receiverMiddle", "receiverBase"] {
            assert!(
                source
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|frame| frame["functionName"] == name
                        && frame["scriptName"]
                            .as_str()
                            .is_some_and(|url| url.ends_with("receiver-inline-setup.js"))),
                "actual inlined source {name}: {source}"
            );
        }
    }

    clear(&trace, &records);
    moving.store(true, Ordering::Relaxed);
    mutate.store(true, Ordering::Relaxed);
    let moved = run(&mut runtime, MOVING, "receiver:native-moving");
    assert_eq!(moved.completion_string(), expected_moving);
    native(&runtime, &trace, &generation, &subjects, &moved);
    {
        let records = records.lock().unwrap();
        let motion = motion(&records);
        assert_eq!(records.bodies.len(), 1);
        let body = records.bodies[0].as_ref().unwrap();
        assert!(body.codes.contains(&generation.code_object_id));
        assert_eq!(
            (body.receiver, body.child),
            (motion.receiver_after, motion.child_after)
        );
    }

    clear(&trace, &records);
    moving.store(true, Ordering::Relaxed);
    let recovered = run(&mut runtime, RECOVER, "receiver:native-recovery");
    assert_eq!(recovered.completion_string(), expected_recovery);
    motion(&records.lock().unwrap());
    let report = recovered.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    let restored: Vec<_> = report
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::InlineDeoptFrame {
                function_id,
                index,
                total,
                resume_pc,
            } => Some((*function_id, *index, *total, *resume_pc)),
            _ => None,
        })
        .collect();
    let middle_byte = call_sites[&middle].iter().find(|site| site.1).unwrap().0;
    let middle_pc = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|region| {
            region["functionId"] == middle
                && region["bytePc"] == middle_byte
                && region["operation"]
                    .as_str()
                    .is_some_and(|op| op.contains("CheckFunctionPrototypeCall"))
        })
        .unwrap()["logicalPc"]
        .as_u64()
        .unwrap() as u32;
    assert_eq!(
        restored,
        [(middle, 1, 2, middle_pc)],
        "exact descendant resumes its original method call; the outer physical frame is retained"
    );
    assert!(report.events().iter().any(|event| matches!(event, JitDebugEvent::EnteredGenerationDeopt { callee_function_id, callee_code_object_id, .. } if *callee_function_id == root && *callee_code_object_id == generation.code_object_id)), "own entered Graph generation performs recovery");
    assert_eq!(
        run(&mut runtime, DOMAINS, "receiver:native-domains").completion_string(),
        expected_domains
    );
}
