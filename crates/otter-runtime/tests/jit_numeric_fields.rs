//! Numeric field words preserve geometry, canonical values and native execution.
//!
//! # Contents
//! - Normal-policy admission of a constructor's direct Float64 append.
//! - Existing field-type widening admits mixed Int32/Double native reads.
//! - Exact generation, source IR, live FP and one-call setter observations.
//!
//! # Invariants
//! - Source programs obtain numeric proofs through ordinary JavaScript operations.
//! - Output observations alone do not stand in for native admission: the exact
//!   installed generation's owned IR and complete dispatch trace are checked.
//! - Stored numeric words become canonical Values at every observed boundary.
//! - Numeric writes do not create a separate shape-format invalidation domain.
//!
//! # See also
//! - `otter-vm/src/object/field_type.rs` owns stored representation summaries.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use otter_runtime::{
    JitArtifactBatch, JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{CodeLifetimeState, NativeFrameKind},
};

#[derive(Default)]
struct Trace {
    names: BTreeMap<u32, String>,
    recording: bool,
    steps: Vec<u32>,
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
            trace.steps.push(event.function_id);
        }
    }
}

fn run(
    runtime: &mut Runtime,
    source: &str,
    name: &str,
    artifacts: &mut Option<JitArtifactBatch>,
) -> otter_runtime::ExecutionResult {
    let mut result = runtime
        .run_script(SourceInput::from_javascript(source), name)
        .expect("numeric field source execution");
    let report = result.jit_debug_report().expect("bounded JIT events");
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    let next = result
        .take_jit_artifacts()
        .expect("enabled compile bundle capture");
    *artifacts = Some(match artifacts.take() {
        Some(previous) => previous.merged(next),
        None => next,
    });
    result
}

fn installed(runtime: &Runtime, fid: u32) -> Option<JitCodeGenerationSnapshot> {
    let mut current = runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .filter(|generation| {
            generation.function_id == fid
                && generation.tier == NativeFrameKind::Optimizing
                && generation.lifecycle == CodeLifetimeState::Installed
                && generation.linked
                && generation.current_entry
        });
    let found = current.next();
    assert!(
        current.next().is_none(),
        "one current Graph entry per function"
    );
    found
}

fn ir_for(artifacts: &Option<JitArtifactBatch>, generation: &JitCodeGenerationSnapshot) -> String {
    let batch = artifacts.as_ref().expect("owned compile bundles");
    assert!(!batch.truncated());
    assert_eq!(batch.dropped_bytes(), 0);
    let bundle = batch
        .bundles()
        .iter()
        .find(|bundle| {
            bundle.manifest().code_object_id() == generation.code_object_id
                && bundle.manifest().function_id() == generation.function_id
        })
        .expect("exact installed generation's compile bundle");
    std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("Graph optimized IR")
            .contents(),
    )
    .unwrap()
    .to_owned()
}

fn admit(
    runtime: &mut Runtime,
    fid: u32,
    calls: &str,
    artifacts: &mut Option<JitArtifactBatch>,
    accepts: impl Fn(&JitCodeGenerationSnapshot, &str) -> bool,
) -> JitCodeGenerationSnapshot {
    for attempt in 0..=64 {
        if let Some(generation) = installed(runtime, fid) {
            let ir = ir_for(artifacts, &generation);
            if accepts(&generation, &ir) {
                assert!(
                    generation.call_entry_offset.is_some(),
                    "actual private-JS entry"
                );
                return generation;
            }
            if attempt == 64 {
                let path = std::env::temp_dir().join(format!(
                    "otter-numeric-fields-{}-{}.ir",
                    std::process::id(),
                    generation.code_object_id
                ));
                std::fs::write(&path, ir).unwrap();
                panic!(
                    "installed generation lacks required numeric lowering; IR {}",
                    path.display()
                );
            }
        }
        assert!(attempt < 64, "bounded normal-policy numeric admission");
        run(
            runtime,
            &calls.repeat(64),
            &format!("numeric-admission-{fid}-{attempt}.js"),
            artifacts,
        );
    }
    unreachable!()
}

fn begin_trace(trace: &Arc<Mutex<Trace>>) {
    let mut trace = trace.lock().unwrap();
    trace.recording = true;
    trace.steps.clear();
}
fn assert_native(trace: &Arc<Mutex<Trace>>, functions: &[u32]) {
    let mut trace = trace.lock().unwrap();
    assert!(!trace.steps.is_empty(), "complete script dispatch trace");
    assert!(
        functions.iter().all(|fid| !trace.steps.contains(fid)),
        "observed source functions stayed native: {:?}",
        trace.steps
    );
    trace.recording = false;
}

#[test]
fn graph_numeric_words_keep_geometry_read_old_int32_peers_and_commit_setter_once() {
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::events().with_artifacts(true))
        .build()
        .expect("production numeric-field runtime");
    let trace = Arc::new(Mutex::new(Trace::default()));
    runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    let mut artifacts = None;
    run(
        &mut runtime,
        r#"
        function NumericAppendBox(x) { this.x = x || 0; this.y = 7; }
        var numericLast, numericSibling, numericWarm;
        var numericArgCalls = 0, numericSetterCalls = 0, numericSetterValue;
        var numericArgumentValue = 0.5;
        var numericSink = 0, numericPeers = [];
        function numericArgument() { numericArgCalls++; return numericArgumentValue; }
        function numericFactory(number) {
            const live = number * 1.25;
            const box = new NumericAppendBox(numericArgument() * 1.25);
            numericLast = box;
            return live + 1;
        }
        function numericPeer() { return {x: 0, y: 7}; }
        function numericRead(box, number) {
            const live = number * 1.25;
            return box.x + box.y + live;
        }
        for (let warm = 0; warm < 20000; warm++) {
            numericSink += numericFactory(3.5);
            numericWarm = numericPeer();
            numericSink += numericRead(numericWarm, 3.5);
        }
        for (let peer = 0; peer < 64; peer++) numericPeers.push(numericPeer());
        numericSibling = numericLast;
    "#,
        "numeric-fields-setup.js",
        &mut artifacts,
    );
    let names = trace.lock().unwrap().names.clone();
    let fid = |name: &str| {
        *names
            .iter()
            .find(|(_, actual)| actual.as_str() == name)
            .unwrap_or_else(|| panic!("actual source identity {name}: {names:?}"))
            .0
    };
    let factory = fid("numericFactory");
    let constructor = fid("NumericAppendBox");
    let reader = fid("numericRead");
    let factory_generation = admit(
        &mut runtime,
        factory,
        "numericFactory(3.5);\n",
        &mut artifacts,
        |_, ir| {
            ir.lines().any(|line| {
                line.contains("StoreNamedProperty") && line.contains("TransitionDouble(")
            })
        },
    );
    // The factory reads none of the mutated fields, so existing FieldType read
    // dependencies cannot obscure a spurious representation invalidation.
    run(
        &mut runtime,
        "numericSibling.x = 1.5; numericSibling.x = null; numericArgumentValue = -0;",
        "numeric-fields-sibling-writes.js",
        &mut artifacts,
    );
    begin_trace(&trace);
    assert_eq!(
        run(
            &mut runtime,
            r#"
        numericArgCalls = 0;
        JSON.stringify([numericFactory(3.5), numericArgCalls, numericLast.x, numericLast.y]);
    "#,
            "numeric-fields-native-factory.js",
            &mut artifacts
        )
        .completion_string(),
        "[5.375,1,0,7]"
    );
    assert_native(&trace, &[factory, constructor]);
    assert_eq!(
        installed(&runtime, factory).unwrap().code_object_id,
        factory_generation.code_object_id,
        "numeric sibling history leaves geometry-only append installed"
    );
    assert_eq!(
        run(
            &mut runtime,
            r#"{
        const keys = new Map([[0, "zero"], [7, "seven"]]);
        const numbers = new Set([0, 7, numericLast.x, numericLast.y]);
        Object.defineProperty(numericLast, "x", {enumerable: false});
        Object.setPrototypeOf(numericLast, {inherited: 31});
        JSON.stringify([Object.is(numericLast.x, 0), keys.get(numericLast.x),
            keys.get(numericLast.y), numbers.size, Object.keys(numericLast).join(","),
            numericLast.inherited]);
    }"#,
            "numeric-fields-canonical-values.js",
            &mut artifacts
        )
        .completion_string(),
        "[true,\"zero\",\"seven\",2,\"y\",31]"
    );

    let initial_reader = admit(
        &mut runtime,
        reader,
        "numericRead(numericWarm, 3.5);\n",
        &mut artifacts,
        |_, _| true,
    );
    run(
        &mut runtime,
        "numericWarm.x = 1.5;",
        "numeric-fields-widen.js",
        &mut artifacts,
    );
    let mixed = admit(
        &mut runtime,
        reader,
        "numericRead(numericWarm, 3.5);\n",
        &mut artifacts,
        |generation, ir| {
            generation.code_object_id != initial_reader.code_object_id
                && ir.lines().any(|line| {
                    line.contains("LoadNumberOwnField") && line.contains("double_only: false")
                })
        },
    );
    begin_trace(&trace);
    assert_eq!(
        run(
            &mut runtime,
            r#"
        numericSink = 0;
        for (let peer = 0; peer < numericPeers.length; peer++) {
            numericSink += numericRead(numericPeers[peer], 3.5);
        }
        JSON.stringify([numericSink, numericWarm.x, numericPeers[0].x, numericPeers[63].y]);
    "#,
            "numeric-fields-native-mixed-peers.js",
            &mut artifacts
        )
        .completion_string(),
        "[728,1.5,0,7]"
    );
    assert_native(&trace, &[reader]);
    assert_eq!(
        installed(&runtime, reader).unwrap().code_object_id,
        mixed.code_object_id,
        "mixed numeric loads accept old Int32 peers without migration or retirement"
    );

    begin_trace(&trace);
    assert_eq!(
        run(
            &mut runtime,
            r#"{
        const results = [];
        for (const number of [NaN, Infinity, -Infinity]) {
            numericWarm.x = number;
            results.push(Object.is(numericRead(numericWarm, 3.5), number));
        }
        JSON.stringify(results);
    }"#,
            "numeric-fields-native-special-numbers.js",
            &mut artifacts,
        )
        .completion_string(),
        "[true,true,true]"
    );
    assert_native(&trace, &[reader]);
    assert_eq!(
        installed(&runtime, reader).unwrap().code_object_id,
        mixed.code_object_id,
        "complete numeric bounds include NaN and infinities"
    );

    assert_eq!(
        run(
            &mut runtime,
            r#"{
        Object.defineProperty(NumericAppendBox.prototype, "x", {
            configurable: true,
            get() { return 11; },
            set(value) { numericSetterCalls++; numericSetterValue = value; }
        });
        numericArgCalls = 0;
        numericSetterCalls = 0;
        const result = numericFactory(3.5);
        JSON.stringify([result, numericArgCalls, numericSetterCalls, numericSetterValue,
            Object.hasOwn(numericLast, "x"), numericLast.y]);
    }"#,
            "numeric-fields-prototype-setter.js",
            &mut artifacts
        )
        .completion_string(),
        "[5.375,1,1,0,false,7]"
    );
}
