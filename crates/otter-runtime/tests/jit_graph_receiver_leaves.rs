//! Own Graph pure receiver leaves with evaluated inputs and actual moving GC.
//!
//! # Contents
//! - Exact current CallWithThis regions, identity guards and typed leaf edges.
//! - Argument-evaluation collection and later method replacement before a hit.
//! - Primitive coercion, Symbol, Proxy and thrown-identity canonical misses.
//! - Default/additional source realms and complete dispatch observations.
//!
//! # Invariants
//! The computed Get and argument callback precede the leaf. A miss resumes the
//! call PC, never either earlier effect. Native callbacks retain owned scalar
//! observations and report failures after returning across the ABI. A live own
//! generation lease and its exact artifact prove execution independently of
//! installed-code counters. No synthetic LAB or GC root window is used.
//!
//! # See also
//! - `otter-jit/src/graph/native_leaf/tests.rs` for both call-clobber contracts.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use otter_bytecode::Op;
use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest,
    JitDebugTarget, JitDebugTier, JitSelection, Runtime, RuntimeExtensionInstaller,
    RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError, RuntimeRealmId, RuntimeValue,
    SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{self as abi, CodeLifetimeState, NativeFrameKind},
};
use serde_json::Value as Json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

const SETUP: &str = r#"
const leafOriginal = String.prototype.charCodeAt;
let leafGets = 0, leafApplies = 0, leafConversions = 0;
globalThis.leafMutate = function leafMutate() { String.prototype.charCodeAt = function leafReplacement() { return 909; }; };
function leafWorker(receiver, key, index, child) {
  const result = receiver[key](leafEvaluate(index, receiver, child));
  leafObserve(result, receiver, index, child);
  return result;
}
const leafWarmChild = {marker: 731};
for (let warm = 0; warm < 20000; warm++) leafWorker('qz', 'charCodeAt', 1, leafWarmChild);
"#;
const HIT: &str = r#"
leafWorker('qz', 'charCodeAt', 1, leafWarmChild);
"#;
const MOVING: &str = r#"
const leafYoungText = ['q', 'z'].join('');
const leafYoungChild = {marker: 731};
const leafChildAlias = leafYoungChild;
const leafMovedResult = leafWorker(leafYoungText, 'charCodeAt', 1, leafYoungChild);
const leafChangedResult = 'qz'.charCodeAt(1);
String.prototype.charCodeAt = leafOriginal;
JSON.stringify([leafMovedResult, leafChangedResult, leafChildAlias === leafYoungChild, leafYoungChild.marker, leafYoungText]);
"#;
const MISS: &str = r#"
const leafCoercion = {valueOf() { leafConversions++; return 1; }};
let leafIdentity = false, leafSymbolIndex = false, leafSymbolReceiver = false, leafProxyReceiver = false;
const leafObjectResult = leafWorker('qz', 'charCodeAt', leafCoercion, leafWarmChild);
const leafFraction = leafWorker('qz', 'charCodeAt', 1.5, leafWarmChild);
const leafOutside = leafWorker('qz', 'charCodeAt', 99, leafWarmChild);
try { leafWorker('qz', 'charCodeAt', Symbol('index'), leafWarmChild); }
catch (error) { leafSymbolIndex = error instanceof TypeError; }
Symbol.prototype.charCodeAt = leafOriginal;
try { leafWorker(Symbol('receiver'), 'charCodeAt', 1, leafWarmChild); }
catch (error) { leafSymbolReceiver = error instanceof TypeError; }
delete Symbol.prototype.charCodeAt;
try { leafWorker(new Proxy(new String('qz'), {}), 'charCodeAt', 1, leafWarmChild); }
catch (error) { leafProxyReceiver = error instanceof TypeError; }
const leafThrow = {marker: 731};
try { leafWorker('qz', 'charCodeAt', {valueOf() { leafConversions++; leafCollectThrow(leafThrow); throw leafThrow; }}, leafWarmChild); }
catch (error) { leafIdentity = error === leafThrow && error.marker === 731; }
const leafLoaded = {get charCodeAt() { leafGets++; return new Proxy(leafOriginal, {apply(target, receiver, args) { leafApplies++; return Reflect.apply(target, 'qz', args); }}); }};
const leafProxyCall = leafWorker(leafLoaded, 'charCodeAt', 1, leafWarmChild);
JSON.stringify([leafObjectResult, leafFraction, Number.isNaN(leafOutside), leafSymbolIndex, leafSymbolReceiver, leafProxyReceiver, leafIdentity, leafProxyCall, leafGets, leafApplies, leafConversions]);
"#;

#[derive(Default)]
struct Trace {
    names: BTreeMap<u32, String>,
    calls: BTreeMap<u32, Vec<u32>>,
    recording: bool,
    ticks: Vec<(u32, u32)>,
    steps: usize,
}
struct Tracer(Arc<Mutex<Trace>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self.0.lock().unwrap();
        trace
            .names
            .entry(event.function_id)
            .or_insert_with(|| event.function_name.to_owned());
        if event.op == Op::CallWithThis {
            let sites = trace.calls.entry(event.function_id).or_default();
            if !sites.contains(&event.byte_pc) {
                sites.push(event.byte_pc);
            }
        }
        if trace.recording {
            trace.steps += 1;
            if trace.ticks.len() < 4096 {
                trace.ticks.push((event.function_id, event.byte_pc));
            }
        }
    }
}
#[derive(Debug)]
struct Motion {
    before_text: u32,
    after_text: u32,
    before_child: u32,
    after_child: u32,
    marker: f64,
    minor_before: u64,
    minor_after: u64,
    updates_before: u64,
    updates_after: u64,
}
#[derive(Debug)]
struct Observation {
    code: u64,
    fid: u32,
    result: f64,
    text: u32,
    child: u32,
}
#[derive(Default)]
struct Records {
    motions: Vec<Result<Motion, String>>,
    observations: Vec<Result<Observation, String>>,
    evaluations: usize,
    throw_motion: Vec<Result<(u32, u32), String>>,
}
fn installer(
    recording: Arc<AtomicBool>,
    moving: Arc<AtomicBool>,
    records: Arc<Mutex<Records>>,
) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        realm.install_native_global_call(
            "leafOffset",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                |_ctx: &mut RuntimeNativeCtx<'_>,
                 args: &[RuntimeValue],
                 _state: &[RuntimeValue]| {
                    let value = args
                        .first()
                        .copied()
                        .ok_or_else(|| RuntimeNativeError::InvalidOperand)?;
                    let offset = value
                        .as_string_gc()
                        .map(|cell| cell.offset())
                        .or_else(|| value.as_object().map(|cell| cell.offset()))
                        .ok_or(RuntimeNativeError::InvalidOperand)?;
                    Ok(RuntimeValue::number_i32(offset as i32))
                },
            )),
        )?;
        realm.install_native_global_call(
            "leafCollect",
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
        let arm = moving.clone();
        let record = recording.clone();
        realm.install_native_global_call(
            "leafEvaluate",
            3,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    if record.load(Ordering::Relaxed) {
                        evaluations.lock().unwrap().evaluations += 1;
                    }
                    if !arm.swap(false, Ordering::Relaxed) {
                        return Ok(args
                            .first()
                            .copied()
                            .unwrap_or_else(RuntimeValue::undefined));
                    }
                    let stats_before = ctx.interp_mut().gc_stats_snapshot();
                    let observed = ctx.scope(|mut scope| -> Result<_, RuntimeNativeError> {
                        let index = scope.argument(args, 0);
                        let receiver = scope.argument(args, 1);
                        let child = scope.argument(args, 2);
                        let offset = scope
                            .global("leafOffset")
                            .ok_or(RuntimeNativeError::InvalidOperand)?;
                        let collect = scope
                            .global("leafCollect")
                            .ok_or(RuntimeNativeError::InvalidOperand)?;
                        let mutate = scope
                            .global("leafMutate")
                            .ok_or(RuntimeNativeError::InvalidOperand)?;
                        let undefined = scope.undefined();
                        let before = scope.call(offset, undefined, &[receiver])?;
                        let before_text = scope.number_value(before)? as i32 as u32;
                        let before = scope.call(offset, undefined, &[child])?;
                        let before_child = scope.number_value(before)? as i32 as u32;
                        // Offset reads above do not collect. All three actual values
                        // remain in handles while the collector and JS mutation run.
                        scope.call(collect, undefined, &[])?;
                        scope.call(mutate, undefined, &[])?;
                        let after = scope.call(offset, undefined, &[receiver])?;
                        let after_text = scope.number_value(after)? as i32 as u32;
                        let after = scope.call(offset, undefined, &[child])?;
                        let after_child = scope.number_value(after)? as i32 as u32;
                        let marker = scope.get(child, "marker")?;
                        let marker = scope.number_value(marker)?;
                        let observation = Motion {
                            before_text,
                            after_text,
                            before_child,
                            after_child,
                            marker,
                            minor_before: stats_before.minor_gc_cycles,
                            minor_after: 0,
                            updates_before: stats_before.minor_slot_updates,
                            updates_after: 0,
                        };
                        let returned = scope.finish(index);
                        Ok((observation, returned))
                    });
                    match observed {
                        Ok((mut motion, value)) => {
                            let after = ctx.interp_mut().gc_stats_snapshot();
                            motion.minor_after = after.minor_gc_cycles;
                            motion.updates_after = after.minor_slot_updates;
                            evaluations.lock().unwrap().motions.push(Ok(motion));
                            Ok(value)
                        }
                        Err(error) => {
                            evaluations
                                .lock()
                                .unwrap()
                                .motions
                                .push(Err(error.to_string()));
                            Err(error)
                        }
                    }
                },
            )),
        )?;
        let observe = records.clone();
        let record = recording.clone();
        realm.install_native_global_call(
            "leafObserve",
            4,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    if record.load(Ordering::Relaxed) {
                        let observed = (|| -> Result<Observation, String> {
                            let generations = ctx.interp_mut().jit_code_generation_snapshot();
                            let own: Vec<_> = generations
                                .iter()
                                .filter(|g| {
                                    g.tier == NativeFrameKind::Optimizing
                                        && g.lifecycle == CodeLifetimeState::Installed
                                        && g.linked
                                        && g.current_entry
                                        && g.active_count > 0
                                })
                                .collect();
                            if own.len() != 1 {
                                return Err(format!("one own active Graph worker: {own:?}"));
                            }
                            let result = args
                                .first()
                                .and_then(|v| v.as_f64())
                                .ok_or("missing leaf result")?;
                            let text = args
                                .get(1)
                                .and_then(|v| v.as_string_gc())
                                .ok_or("missing current text")?
                                .offset();
                            let child = args
                                .get(3)
                                .and_then(|v| v.as_object())
                                .ok_or("missing current child")?
                                .offset();
                            Ok(Observation {
                                code: own[0].code_object_id,
                                fid: own[0].function_id,
                                result,
                                text,
                                child,
                            })
                        })();
                        observe.lock().unwrap().observations.push(observed);
                    }
                    Ok(RuntimeValue::undefined())
                },
            )),
        )?;
        let throws = records.clone();
        realm.install_native_global_call(
            "leafCollectThrow",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    let motion = ctx.scope(|mut scope| -> Result<_, RuntimeNativeError> {
                        let value = scope.argument(args, 0);
                        let undefined = scope.undefined();
                        let offset = scope
                            .global("leafOffset")
                            .ok_or(RuntimeNativeError::InvalidOperand)?;
                        let collect = scope
                            .global("leafCollect")
                            .ok_or(RuntimeNativeError::InvalidOperand)?;
                        let before = scope.call(offset, undefined, &[value])?;
                        let before = scope.number_value(before)? as i32 as u32;
                        scope.call(collect, undefined, &[])?;
                        let after = scope.call(offset, undefined, &[value])?;
                        let after = scope.number_value(after)? as i32 as u32;
                        Ok((before, after))
                    });
                    match motion {
                        Ok(motion) => {
                            throws.lock().unwrap().throw_motion.push(Ok(motion));
                            Ok(RuntimeValue::undefined())
                        }
                        Err(error) => {
                            throws
                                .lock()
                                .unwrap()
                                .throw_motion
                                .push(Err(error.to_string()));
                            Err(error)
                        }
                    }
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
    .expect("receiver-leaf source completes")
}
fn current(runtime: &Runtime, fid: u32) -> JitCodeGenerationSnapshot {
    let found: Vec<_> = runtime
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
    assert_eq!(found.len(), 1, "one own current Graph entry: {found:?}");
    found.into_iter().next().unwrap()
}
fn json(bundle: &JitArtifactBundle, name: JitArtifactFileName) -> Json {
    serde_json::from_slice(bundle.file(name).unwrap().contents()).unwrap()
}
fn artifact<'a>(
    batch: &'a JitArtifactBatch,
    generation: &JitCodeGenerationSnapshot,
) -> &'a JitArtifactBundle {
    let bundle = batch
        .bundles()
        .iter()
        .find(|b| b.manifest().code_object_id() == generation.code_object_id)
        .expect("own current artifact");
    assert_eq!(bundle.manifest().function_id(), generation.function_id);
    assert_eq!(bundle.manifest().tier(), JitDebugTier::Optimizing);
    assert_eq!(bundle.manifest().entry(), JitDebugTarget::Entry);
    bundle
}
fn reset(trace: &Arc<Mutex<Trace>>, records: &Arc<Mutex<Records>>) {
    let mut t = trace.lock().unwrap();
    t.recording = true;
    t.steps = 0;
    t.ticks.clear();
    *records.lock().unwrap() = Records::default();
}
fn native_phase(
    runtime: &Runtime,
    trace: &Arc<Mutex<Trace>>,
    records: &Arc<Mutex<Records>>,
    before: &JitCodeGenerationSnapshot,
    result: &otter_runtime::ExecutionResult,
) {
    let t = trace.lock().unwrap();
    assert!(t.steps > 0);
    assert_eq!(t.steps, t.ticks.len(), "complete dispatch trace");
    assert!(
        !t.ticks.iter().any(|(fid, _)| *fid == before.function_id),
        "worker must stay native: {:?}",
        t.ticks
    );
    let after = current(runtime, before.function_id);
    assert_eq!(after.code_object_id, before.code_object_id);
    assert_eq!(after.generated_deopts, before.generated_deopts);
    assert_eq!(after.active_count, 0);
    let records = records.lock().unwrap();
    assert_eq!(records.evaluations, 1);
    assert_eq!(records.observations.len(), 1);
    let observed = records.observations[0].as_ref().unwrap();
    assert_eq!(
        (observed.code, observed.fid),
        (before.code_object_id, before.function_id)
    );
    assert_eq!(observed.result, 122.0);
    let report = result.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    assert!(
        !report.events().iter().any(|e| matches!(
            e,
            JitDebugEvent::Bail { .. }
                | JitDebugEvent::EnteredGenerationDeopt { .. }
                | JitDebugEvent::InlineDeoptFrame { .. }
                | JitDebugEvent::CompilePrepared { .. }
        )),
        "unexpected native-phase exit: {:?}",
        report.events()
    );
}
fn case(extra: bool) {
    let recording = Arc::new(AtomicBool::new(false));
    let moving = Arc::new(AtomicBool::new(false));
    let records = Arc::new(Mutex::new(Records::default()));
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(installer(
            recording.clone(),
            moving.clone(),
            records.clone(),
        ))
        .build()
        .unwrap();
    let realm = extra.then(|| runtime.create_realm().unwrap());
    let trace = Arc::new(Mutex::new(Trace::default()));
    runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    let mut setup = run(&mut runtime, realm, SETUP, "graph-receiver-leaf-setup.js");
    let fid = *trace
        .lock()
        .unwrap()
        .names
        .iter()
        .find(|(_, name)| name.as_str() == "leafWorker")
        .unwrap()
        .0;
    let mut batch = setup.take_jit_artifacts().unwrap();
    for attempt in 0..=128 {
        if runtime.jit_code_generation_snapshot().iter().any(|g| {
            g.function_id == fid
                && g.tier == NativeFrameKind::Optimizing
                && g.lifecycle == CodeLifetimeState::Installed
                && g.linked
                && g.current_entry
        }) {
            break;
        }
        assert!(attempt < 128, "bounded actual entry admission");
        let mut admitted = run(
            &mut runtime,
            realm,
            &HIT.repeat(16),
            &format!("graph-receiver-leaf-admit-{attempt}.js"),
        );
        batch = batch.merged(admitted.take_jit_artifacts().unwrap());
    }
    assert!(!batch.truncated());
    let generation = current(&runtime, fid);
    let bundle = artifact(&batch, &generation);
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().unwrap();
    let leaf = regions
        .iter()
        .find(|r| {
            r["functionId"] == fid
                && r["operation"]
                    .as_str()
                    .is_some_and(|op| op.contains("NativeLeaf"))
        })
        .expect("own specialized CallWithThis");
    let call_pc = leaf["bytePc"].as_u64().unwrap() as u32;
    assert!(
        trace.lock().unwrap().calls[&fid].contains(&call_pc),
        "actual dispatched source opcode is CallWithThis"
    );
    assert!(leaf["startOffset"].as_u64() < leaf["endOffset"].as_u64());
    assert!(regions.iter().any(|r| {
        r["functionId"] == fid
            && r["bytePc"] == call_pc
            && r["operation"]
                .as_str()
                .is_some_and(|op| op.contains("CheckNative"))
    }));
    assert!(!regions.iter().any(|r| {
        r["functionId"] == fid
            && r["bytePc"] == call_pc
            && r["operation"]
                .as_str()
                .is_some_and(|op| op.contains("Generic") || op.contains("CallJs"))
    }));
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let edges: Vec<_> = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| {
            r["target"]["kind"] == "runtimeStub"
                && r["startOffset"].as_u64() >= leaf["startOffset"].as_u64()
                && r["startOffset"].as_u64() < leaf["endOffset"].as_u64()
        })
        .collect();
    assert_eq!(
        edges.len(),
        1,
        "one VM leaf entry in the specialized region"
    );
    assert_eq!(
        edges[0]["target"]["id"],
        abi::STUB_STRING_CHAR_CODE_AT_LEAF.id
    );
    assert_eq!(edges[0]["target"]["signature"], "leafValue2");
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let after_load = edges[0]["endOffset"].as_u64().unwrap() as usize;
    #[cfg(target_arch = "aarch64")]
    assert_eq!(
        &code[after_load..after_load + 4],
        &[0x00, 0x02, 0x3f, 0xd6],
        "actual BLR x16 leaf edge"
    );
    #[cfg(target_arch = "x86_64")]
    {
        let end = leaf["endOffset"].as_u64().unwrap() as usize;
        assert_eq!(
            code[after_load..end]
                .windows(3)
                .filter(|bytes| bytes.starts_with(&[0x41, 0xff, 0xd3]))
                .count(),
            1,
            "actual CALL r11 leaf edge, including platform call area"
        );
        #[cfg(not(target_os = "windows"))]
        assert_eq!(&code[after_load..after_load + 3], &[0x41, 0xff, 0xd3]);
    }
    let points = json(bundle, JitArtifactFileName::Safepoints);
    assert!(!points["returnSites"].as_array().unwrap().iter().any(|s| {
        s["nativeReturnOffset"].as_u64() >= leaf["startOffset"].as_u64()
            && s["nativeReturnOffset"].as_u64() < leaf["endOffset"].as_u64()
    }));
    let oracle_moving = Arc::new(AtomicBool::new(false));
    let mut oracle = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .extension_installer(installer(
            Arc::new(AtomicBool::new(false)),
            oracle_moving.clone(),
            Arc::new(Mutex::new(Records::default())),
        ))
        .build()
        .unwrap();
    let oracle_realm = extra.then(|| oracle.create_realm().unwrap());
    run(
        &mut oracle,
        oracle_realm,
        SETUP,
        "graph-receiver-leaf-setup.js",
    );
    let expected_hit = run(&mut oracle, oracle_realm, HIT, "graph-receiver-leaf-hit.js")
        .completion_string()
        .to_owned();
    oracle_moving.store(true, Ordering::Relaxed);
    let expected_moving = run(
        &mut oracle,
        oracle_realm,
        MOVING,
        "graph-receiver-leaf-moving.js",
    )
    .completion_string()
    .to_owned();
    let expected_miss = run(
        &mut oracle,
        oracle_realm,
        MISS,
        "graph-receiver-leaf-misses.js",
    )
    .completion_string()
    .to_owned();
    recording.store(true, Ordering::Relaxed);
    reset(&trace, &records);
    let hit = run(&mut runtime, realm, HIT, "graph-receiver-leaf-hit.js");
    assert_eq!(hit.completion_string(), expected_hit);
    native_phase(&runtime, &trace, &records, &generation, &hit);
    reset(&trace, &records);
    moving.store(true, Ordering::Relaxed);
    let moved = run(&mut runtime, realm, MOVING, "graph-receiver-leaf-moving.js");
    assert_eq!(expected_moving, "[122,909,true,731,\"qz\"]");
    assert_eq!(moved.completion_string(), expected_moving);
    native_phase(&runtime, &trace, &records, &generation, &moved);
    {
        let r = records.lock().unwrap();
        assert_eq!(r.motions.len(), 1);
        let motion = r.motions[0].as_ref().unwrap();
        assert_ne!(
            motion.before_text, motion.after_text,
            "actual string evacuation"
        );
        assert_ne!(
            motion.before_child, motion.after_child,
            "actual child evacuation"
        );
        assert_eq!(motion.marker, 731.0);
        assert!(
            motion.minor_after > motion.minor_before
                && motion.updates_after > motion.updates_before
        );
        let observed = r.observations[0].as_ref().unwrap();
        assert_eq!(observed.text, motion.after_text);
        assert_eq!(observed.child, motion.after_child);
    }
    // Only the first refused operand is observed natively: its eager miss
    // resumes the original call. The rest deliberately exercises canonical
    // completed consumers after that entry exits.
    recording.store(false, Ordering::Relaxed);
    reset(&trace, &records);
    let missed = run(&mut runtime, realm, MISS, "graph-receiver-leaf-misses.js");
    assert_eq!(
        expected_miss,
        "[122,122,true,true,true,true,true,122,1,1,2]"
    );
    assert_eq!(missed.completion_string(), expected_miss);
    let t = trace.lock().unwrap();
    assert!(t.steps > 0);
    assert_eq!(t.steps, t.ticks.len());
    assert_eq!(
        t.ticks.iter().find(|(id, _)| *id == fid).map(|(_, pc)| *pc),
        Some(call_pc),
        "first fallback resumes CallWithThis after the already evaluated Get/argument"
    );
    let r = records.lock().unwrap();
    assert_eq!(r.throw_motion.len(), 1);
    let &(before, after) = r.throw_motion[0].as_ref().unwrap();
    assert_ne!(
        before, after,
        "original thrown identity survives actual collection"
    );
}
#[test]
fn own_graph_receiver_leaf_hits_and_canonical_misses_keep_current_source_and_moving_roots() {
    for extra in [false, true] {
        case(extra);
    }
}
