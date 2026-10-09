//! Own-generation receiver fits followed by collecting callee Array construction.
//!
//! # Contents
//! - Real closure receiver LAB hits and canonical misses in the same native edge.
//! - Exact current Graph artifacts, complete interpreter traces and lease release.
//! - Late stress activation around the production ArrayConstruct allocation.
//!
//! # Invariants
//! - Native observation retains only offsets; it never roots or keeps raw cells.
//! - A late allocation moves the exact receiver while its child aliases remain
//!   live in native argument, field and canonical frame homes.
//! - The collecting allocation is the tested generated callee's ArrayConstruct;
//!   no separate force-GC operation substitutes for it.
//! - A private host-buffer unit fit cannot satisfy the runtime LAB-hit assertion.
//!
//! # See also
//! - The parent fixture owns exact generation and private entry-cell proofs.
//! - `otter_vm::constructor_layout` traces the exact family and original
//!   receiver through canonical preparation, frame completion and moving GC.

use super::*;
use otter_runtime::{
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError,
    RuntimeValue,
};
use std::sync::atomic::{AtomicBool, Ordering};

const MODULE: &str = "jit-native-receiver-own-generation.js";
const PROBE: &str = r#"
const receiverProbeChild = {marker: 7};
const receiverOne = buildReceiverWithArray(ReceiverCtor, receiverProbeChild, receiverObserve);
const receiverTwo = buildReceiverWithArray(ReceiverCtor, receiverProbeChild, receiverObserve);
JSON.stringify([receiverOne.child === receiverProbeChild, receiverOne.marker, receiverOne.array.length,
                receiverTwo.child === receiverProbeChild, receiverTwo.marker, receiverTwo.array.length,
                receiverOne !== receiverTwo,
                Object.getPrototypeOf(receiverOne) === ReceiverCtor.prototype,
                Object.getPrototypeOf(receiverTwo) === ReceiverCtor.prototype]);
"#;
const SETUP: &str = r#"
function receiverFactory() {
  return function ReceiverWithArray(child, observe) {
    this.child = child;
    this.marker = child.marker;
    observe(this, child, 0);
    this.array = new Array(1);
    observe(this, child, 1);
  };
}
const ReceiverCtor = receiverFactory();
const receiverWarmChild = {marker: 7};
function buildReceiverWithArray(Ctor, child, observe) { return new Ctor(child, observe); }
for (let warm = 0; warm < 20000; warm++) {
  buildReceiverWithArray(ReceiverCtor, receiverWarmChild, receiverObserve);
}
"#;

#[derive(Debug)]
struct Observation {
    phase: i32,
    receiver: u32,
    child: u32,
}

fn assert_native_probe(
    runtime: &Runtime,
    trace: &ArrayProbeTrace,
    generations: &[JitCodeGenerationSnapshot],
    probe: &otter_runtime::ExecutionResult,
    delta: CounterDelta,
) {
    assert_eq!(
        probe.completion_string(),
        "[true,7,1,true,7,1,true,true,true]"
    );
    assert_complete_probe_trace(trace);
    for generation in generations {
        assert!(
            !trace
                .ticks
                .iter()
                .any(|(fid, _, _, _)| *fid == generation.function_id),
            "the exact own constructor/caller must stay native: {generation:?}"
        );
        let after = current_graph(runtime, generation.function_id);
        assert_eq!(after.code_object_id, generation.code_object_id);
        assert_eq!(after.generated_deopts, generation.generated_deopts);
        assert_eq!(after.active_count, 0);
    }
    let report = probe.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    assert!(!report.events().iter().any(|event| matches!(
        event,
        JitDebugEvent::Bail { .. }
            | JitDebugEvent::EnteredGenerationDeopt { .. }
            | JitDebugEvent::InlineDeoptFrame { .. }
            | JitDebugEvent::CompilePrepared { .. }
    )));
    assert_eq!(delta.generated_call_deopts, 0);
    assert_eq!(delta.generated_deopts(), 0);
    assert_eq!(delta.optimized_deopts, 0);
    assert_eq!(delta.code_generations, 0);
    assert_eq!(delta.optimized_osr_entries, 0);
    assert_eq!(
        delta.alloc_value_stub_ok, 2,
        "one ArrayConstruct per native callee"
    );
    assert_eq!(delta.alloc_value_stub_miss, 0);
    assert_eq!(delta.alloc_value_stub_out_of_memory, 0);
    assert_eq!(delta.alloc_value_stub_other, 0);
}

#[test]
fn own_generated_receiver_fits_and_collecting_array_construct_keep_exact_roots() {
    let recording = Arc::new(AtomicBool::new(false));
    let observations = Arc::new(Mutex::new(Vec::<Observation>::new()));
    let original_stride = gc_stress_stride();
    let installer: RuntimeExtensionInstaller = {
        let recording = recording.clone();
        let observations = observations.clone();
        RuntimeExtensionInstaller::new(move |realm| {
            let recording = recording.clone();
            let observations = observations.clone();
            realm.install_native_global_call(
                "receiverStress",
                1,
                RuntimeNativeCall::Dynamic(Arc::new(
                    |ctx: &mut RuntimeNativeCtx<'_>,
                     args: &[RuntimeValue],
                     _state: &[RuntimeValue]| {
                        let stride = args
                            .first()
                            .and_then(|value| value.as_number())
                            .ok_or_else(|| RuntimeNativeError::Error {
                                message: "missing receiver stress stride".into(),
                            })?
                            .as_f64() as u32;
                        ctx.interp_mut().gc_heap_mut().set_gc_stress(stride, true);
                        Ok(RuntimeValue::undefined())
                    },
                )),
            )?;
            realm.install_native_global_call(
                "receiverObserve",
                3,
                RuntimeNativeCall::Dynamic(Arc::new(
                    move |_ctx: &mut RuntimeNativeCtx<'_>,
                          args: &[RuntimeValue],
                          _state: &[RuntimeValue]|
                          -> Result<RuntimeValue, RuntimeNativeError> {
                        if recording.load(Ordering::Relaxed) {
                            let phase = args
                                .get(2)
                                .and_then(|value| value.as_number())
                                .ok_or_else(|| RuntimeNativeError::Error {
                                    message: "missing receiver observation phase".into(),
                                })?
                                .as_f64() as i32;
                            // No GC allocation occurs while these borrowed arguments
                            // are inspected; only their scalar offsets escape.
                            let receiver = args
                                .first()
                                .copied()
                                .and_then(RuntimeValue::as_object)
                                .ok_or_else(|| RuntimeNativeError::Error {
                                    message: "missing live receiver".into(),
                                })?
                                .offset();
                            let child = args
                                .get(1)
                                .copied()
                                .and_then(RuntimeValue::as_object)
                                .ok_or_else(|| RuntimeNativeError::Error {
                                    message: "missing live child alias".into(),
                                })?
                                .offset();
                            observations
                                .lock()
                                .map_err(|_| RuntimeNativeError::Error {
                                    message: "receiver observation recorder poisoned".into(),
                                })?
                                .push(Observation {
                                    phase,
                                    receiver,
                                    child,
                                });
                        }
                        Ok(RuntimeValue::undefined())
                    },
                )),
            )
        })
    };
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(installer)
        .build()
        .expect("native receiver proof runtime");
    let trace = Arc::new(Mutex::new(ArrayProbeTrace::default()));
    runtime.set_tracer(Some(Box::new(ArrayProbeTracer(trace.clone()))));
    let mut setup = runtime
        .run_script(SourceInput::from_javascript(SETUP), MODULE)
        .unwrap();
    let names = traced_function_names(
        &trace.lock().unwrap(),
        &["buildReceiverWithArray", "ReceiverWithArray"],
    );
    let artifacts = admit_graph_entries(
        &mut runtime,
        setup.take_jit_artifacts().unwrap(),
        &names,
        "buildReceiverWithArray(ReceiverCtor, receiverWarmChild, receiverObserve);\n",
        "jit-native-receiver-own-admission",
    );
    let generations: Vec<_> = names
        .iter()
        .map(|(_, fid)| current_graph(&runtime, *fid))
        .collect();
    assert_current_graph_call(&artifacts, &generations[0], &generations[1]);
    let ir = std::str::from_utf8(
        current_graph_entry(&artifacts, &generations[0])
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert!(
        ir.lines()
            .any(|line| line.contains("CallJs") && line.contains("allocation: Some")),
        "the exact own construct edge must own a baked receiver allocation plan: {ir}"
    );
    let callee = current_graph_entry(&artifacts, &generations[1]);
    let relocations = artifact_json(callee, JitArtifactFileName::Relocations);
    assert!(
        relocations["relocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["target"]["name"] == "array_construct_alloc")
    );
    assert!(
        relocations["relocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["target"]["name"] == "jit_prepare_activation"),
        "the exact current callee retains canonical receiver preparation on its construct entry"
    );
    drop(setup);

    recording.store(true, Ordering::Relaxed);
    trace.lock().unwrap().recording = true;
    let before = runtime.execution_stats();
    let probe = runtime
        .run_script(
            SourceInput::from_javascript(PROBE),
            "jit-native-receiver-fit-probe.js",
        )
        .unwrap();
    let after = runtime.execution_stats();
    assert_native_probe(
        &runtime,
        &trace.lock().unwrap(),
        &generations,
        &probe,
        CounterDelta::between(before, after),
    );
    let attempts = after.jit_receiver_alloc_attempts - before.jit_receiver_alloc_attempts;
    assert_eq!(
        attempts, 2,
        "both exact own construct edges execute the receiver plan"
    );
    if original_stride == 0 {
        assert!(
            after.jit_receiver_alloc_generated > before.jit_receiver_alloc_generated,
            "an actual generated constructor must publish a receiver LAB fit: {before:?} -> {after:?}"
        );
    } else {
        assert_eq!(
            after.jit_receiver_alloc_generated, before.jit_receiver_alloc_generated,
            "external stress disables the LAB fit"
        );
    }
    assert_eq!(
        observations.lock().unwrap().len(),
        4,
        "each callee observes before and after exactly once"
    );
    drop(probe);

    observations.lock().unwrap().clear();
    {
        let mut trace = trace.lock().unwrap();
        trace.recording = false;
        trace.steps = 0;
        trace.ticks.clear();
    }
    // Configure stress before the measured callee is entered. Stress disables
    // generated LAB fits while the GC-owned constructor family stays live.
    // Both entries use rooted canonical receiver preparation; the real callee
    // ArrayConstruct must move that receiver, preserving its family ticket
    // and live child aliases through the same published native frame.
    run(
        &mut runtime,
        "receiverStress(1);",
        "jit-native-receiver-arm.js",
    );
    trace.lock().unwrap().recording = true;
    let before = runtime.execution_stats();
    let probe = runtime
        .run_script(
            SourceInput::from_javascript(
                PROBE
                    .replace("receiverProbeChild", "receiverCollectChild")
                    .replace("receiverOne", "receiverCollectOne")
                    .replace("receiverTwo", "receiverCollectTwo"),
            ),
            "jit-native-receiver-collecting-probe.js",
        )
        .unwrap();
    let after = runtime.execution_stats();
    assert_native_probe(
        &runtime,
        &trace.lock().unwrap(),
        &generations,
        &probe,
        CounterDelta::between(before, after),
    );
    let observations = observations.lock().unwrap();
    assert_eq!(observations.len(), 4);
    for pair in observations.chunks_exact(2) {
        assert_eq!(pair[0].phase, 0);
        assert_eq!(pair[1].phase, 1);
        assert_ne!(
            pair[0].receiver, pair[1].receiver,
            "the exact callee ArrayConstruct must move its live receiver: {pair:?}"
        );
        assert_ne!(pair[0].receiver, pair[0].child);
        assert_ne!(pair[1].receiver, pair[1].child);
    }
    assert!(after.gc_minor_cycles > before.gc_minor_cycles);
    assert!(after.gc_minor_root_slots_scanned > before.gc_minor_root_slots_scanned);
    assert!(after.gc_minor_slot_updates > before.gc_minor_slot_updates);
    assert_eq!(
        after.jit_receiver_alloc_guard_misses, before.jit_receiver_alloc_guard_misses,
        "the exact finalized family/prototype guards survive both collecting native entries: {before:?} -> {after:?}"
    );
    assert_eq!(
        after.jit_receiver_alloc_generated, before.jit_receiver_alloc_generated,
        "both disabled-LAB misses create distinct ordinary receivers through the callee's emitted canonical preparation"
    );
    // Each construct probes twice: the caller's baked plan misses and passes
    // no receiver, then the callee's construct entry probes the family again
    // before its canonical preparation.
    assert_eq!(
        after.jit_receiver_alloc_attempts - before.jit_receiver_alloc_attempts,
        4
    );
    assert_eq!(
        after.jit_receiver_alloc_space_misses - before.jit_receiver_alloc_space_misses,
        4,
        "every retired-LAB probe reaches rooted canonical preparation; the second construct follows ArrayConstruct movement: {before:?} -> {after:?}"
    );
    drop(observations);
    trace.lock().unwrap().recording = false;
    run(
        &mut runtime,
        format!("receiverStress({original_stride});"),
        "jit-native-receiver-disarm.js",
    );
}
