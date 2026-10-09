//! Actual retained constructor cells and finalized native receiver reuse.
//!
//! # Contents
//! - First seven physical cells stay large after finalization and full GC.
//! - Exact current Graph caller/callee and their finalized receiver allocation.
//! - Actual native LAB fits, or the canonical collecting miss under stress.
//!
//! # Invariants
//! - Observers record scalar cell geometry only and retain no VM handle.
//! - Ordinary source work admits every generation; no tier budget is forced.
//! - Native execution requires exact call artifacts, complete trace exclusion,
//!   real allocation-attempt counters and unchanged installed generations.

use super::*;
use otter_runtime::{
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError,
    RuntimeValue,
};
use std::sync::atomic::{AtomicBool, Ordering};

const MODULE: &str = "jit-constructor-first-seven.js";
const INITIAL: &str = r#"
function layoutFactory() {
  return function LayoutCtor(value, observe) {
    // Reading its actual arguments keeps the constructor out of its caller's
    // body: the caller must enter its own generation with a fitted receiver.
    if (arguments.length !== 2) throw "layout arity";
    this.value = value;
    this.next = value + 1;
    observe(this);
  };
}
const LayoutTarget = layoutFactory();
function layoutBuild(Ctor, value, observe) { return new Ctor(value, observe); }
const layoutRetained = [];
for (let first = 0; first < 7; first++) {
  layoutRetained.push(layoutBuild(LayoutTarget, first, layoutObserve));
}
layoutRetained.length;
"#;
const WARM: &str = r#"
for (let warm = 0; warm < 20000; warm++) {
  layoutBuild(LayoutTarget, warm, layoutObserve);
}
"#;
const PROBE: &str = r#"
const layoutFutureA = layoutBuild(LayoutTarget, 42, layoutObserve);
const layoutFutureB = layoutBuild(LayoutTarget, 57, layoutObserve);
JSON.stringify([layoutFutureA.value, layoutFutureA.next, layoutFutureB.value, layoutFutureB.next,
  layoutFutureA !== layoutFutureB, Object.getPrototypeOf(layoutFutureA) === LayoutTarget.prototype,
  Object.getPrototypeOf(layoutFutureB) === LayoutTarget.prototype]);
"#;
const RETAINED: &str = r#"
layoutObserve(layoutRetained[0]); layoutObserve(layoutRetained[1]);
layoutObserve(layoutRetained[2]); layoutObserve(layoutRetained[3]);
layoutObserve(layoutRetained[4]); layoutObserve(layoutRetained[5]);
layoutObserve(layoutRetained[6]);
JSON.stringify([layoutRetained.length, layoutRetained[0].value, layoutRetained[0].next,
  layoutRetained[6].value, layoutRetained[6].next]);
"#;

fn cell_bytes(capacity: usize) -> usize {
    otter_gc::header::HEADER_SIZE
        + std::mem::size_of::<otter_vm::object::ObjectBody>()
        + capacity * std::mem::size_of::<RuntimeValue>()
}

#[test]
fn retained_first_seven_cells_keep_geometry_while_current_native_calls_use_final_layout() {
    let recording = Arc::new(AtomicBool::new(true));
    let observations = Arc::new(Mutex::new(Vec::<Result<usize, String>>::new()));
    let installer = {
        let recording = recording.clone();
        let observations = observations.clone();
        RuntimeExtensionInstaller::new(move |realm| {
            let recording = recording.clone();
            let observations = observations.clone();
            realm.install_native_global_call(
                "layoutObserve",
                1,
                RuntimeNativeCall::Dynamic(Arc::new(
                    move |ctx: &mut RuntimeNativeCtx<'_>,
                          args: &[RuntimeValue],
                          _state: &[RuntimeValue]|
                          -> Result<RuntimeValue, RuntimeNativeError> {
                        if recording.load(Ordering::Relaxed) {
                            let sample = args
                                .first()
                                .copied()
                                .and_then(RuntimeValue::as_object)
                                .ok_or_else(|| "missing live constructor receiver".to_owned())
                                .and_then(|object| {
                                    ctx.scope(|mut scope| {
                                        let object = scope.value(RuntimeValue::object(object));
                                        scope.object_allocation_bytes(object)
                                    })
                                    .map_err(|error| error.to_string())
                                });
                            observations
                                .lock()
                                .map_err(|_| RuntimeNativeError::Error {
                                    message: "constructor geometry recorder poisoned".into(),
                                })?
                                .push(sample);
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
        .unwrap();
    let trace = Arc::new(Mutex::new(ArrayProbeTrace::default()));
    runtime.set_tracer(Some(Box::new(ArrayProbeTracer(trace.clone()))));
    assert_eq!(run(&mut runtime, INITIAL, MODULE), "7");
    assert_eq!(
        *observations.lock().unwrap(),
        vec![Ok(cell_bytes(64)); 7],
        "each of the first seven actual cells has its original 64-word footprint"
    );
    recording.store(false, Ordering::Relaxed);
    let names = traced_function_names(&trace.lock().unwrap(), &["layoutBuild", "LayoutCtor"]);
    let mut warmed = runtime
        .run_script(
            SourceInput::from_javascript(WARM),
            "jit-constructor-first-seven-warm.js",
        )
        .unwrap();
    let artifacts = admit_graph_entries(
        &mut runtime,
        warmed.take_jit_artifacts().unwrap(),
        &names,
        "layoutBuild(LayoutTarget, 1, layoutObserve);\n",
        "jit-constructor-first-seven-admission",
    );
    drop(warmed);
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
        "current native caller must own the finalized receiver plan: {ir}"
    );
    runtime
        .force_gc()
        .expect("full GC before finalized native reuse");
    observations.lock().unwrap().clear();
    recording.store(true, Ordering::Relaxed);
    assert_eq!(
        run(
            &mut runtime,
            RETAINED,
            "jit-constructor-retained-geometry.js"
        ),
        "[7,0,1,6,7]"
    );
    assert_eq!(
        *observations.lock().unwrap(),
        vec![Ok(cell_bytes(64)); 7],
        "finalization/full GC cannot shrink or repoint retained first-seven cells"
    );
    observations.lock().unwrap().clear();
    trace.lock().unwrap().recording = true;
    let before = runtime.execution_stats();
    let probe = runtime
        .run_script(
            SourceInput::from_javascript(PROBE),
            "jit-constructor-finalized-native-probe.js",
        )
        .unwrap();
    let after = runtime.execution_stats();
    assert_eq!(probe.completion_string(), "[42,43,57,58,true,true,true]");
    assert_eq!(
        *observations.lock().unwrap(),
        vec![Ok(cell_bytes(2)); 2],
        "only future cells use the observed/static two-word finalized footprint"
    );
    assert_complete_probe_trace(&trace.lock().unwrap());
    for generation in &generations {
        assert!(
            !trace
                .lock()
                .unwrap()
                .ticks
                .iter()
                .any(|(fid, _, _, _)| *fid == generation.function_id),
            "exact caller and constructor stay native: {generation:?}"
        );
        let current = current_graph(&runtime, generation.function_id);
        assert_eq!(current.code_object_id, generation.code_object_id);
        assert_eq!(current.generated_deopts, generation.generated_deopts);
        assert_eq!(current.active_count, 0);
    }
    let report = probe.jit_debug_report().unwrap();
    assert!(!report.truncated() && report.dropped_events() == 0);
    assert!(!report.events().iter().any(|event| matches!(
        event,
        JitDebugEvent::Bail { .. }
            | JitDebugEvent::EnteredGenerationDeopt { .. }
            | JitDebugEvent::InlineDeoptFrame { .. }
            | JitDebugEvent::CompilePrepared { .. }
    )));
    assert_eq!(after.jit_code_generations, before.jit_code_generations);
    assert_eq!(
        after.jit_generated_call_deopts,
        before.jit_generated_call_deopts
    );
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(
        after.jit_optimized_osr_entries,
        before.jit_optimized_osr_entries
    );
    assert_eq!(
        after.jit_receiver_alloc_attempts - before.jit_receiver_alloc_attempts,
        2
    );
    if gc_stress_stride() == 0 {
        assert_eq!(
            after.jit_receiver_alloc_generated - before.jit_receiver_alloc_generated,
            2,
            "both current native calls carve the finalized family directly after full GC"
        );
        assert_eq!(
            after.jit_receiver_alloc_guard_misses,
            before.jit_receiver_alloc_guard_misses
        );
    } else {
        assert_eq!(
            after.jit_receiver_alloc_generated,
            before.jit_receiver_alloc_generated
        );
        assert_eq!(
            after.jit_receiver_alloc_guard_misses - before.jit_receiver_alloc_guard_misses,
            2,
            "stress disables LAB fits and canonical allocation preserves the finalized capacity"
        );
        assert!(after.gc_minor_cycles > before.gc_minor_cycles);
    }
    trace.lock().unwrap().recording = false;
    recording.store(false, Ordering::Relaxed);
    drop(probe);
    runtime
        .force_gc()
        .expect("all native constructor frames completed");
    assert_eq!(
        run(
            &mut runtime,
            "layoutFutureA.value + layoutFutureB.next + layoutRetained[6].next;",
            "jit-constructor-finalized-retention.js"
        ),
        "107"
    );
}
