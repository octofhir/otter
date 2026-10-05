//! Executed method and constructor linkage on both native targets.
//!
//! # Contents
//! - Polymorphic inherited methods enter exact current callee generations.
//! - A changed descriptor resolves and invokes its getter exactly once.
//! - Oversized dictionary prototypes preserve inherited method semantics at
//!   the first reached admission site.
//! - Ordinary construction preserves receiver, prototype and new.target.
//!
//! # Invariants
//! - Artifact edges are joined to the installed caller and callee identities.
//! - Fresh non-looping probes have complete bounded interpreter traces.
//! - Native execution requires absent subject dispatch and callee entry deltas.
//! - Interpreter execution independently supplies each semantic result.
//!
//! # See also
//! - `otter-jit::template` for emitted call-link diagnostics.
//! - `jit_source_work` for independently measured source-opcode accounting.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use otter_bytecode::Op;
use otter_runtime::{
    JitArtifactBatch, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitDebugTarget,
    JitSelection, Runtime, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot, JitDirectCallKind, JitDirectCallLoweringOutcome,
    native_abi::{CodeLifetimeState, NativeFrameKind},
};

#[path = "support/return_sites.rs"]
mod return_sites;

#[derive(Default)]
struct Trace {
    names: BTreeMap<u32, String>,
    recording: bool,
    count: usize,
    ticks: Vec<(u32, Op)>,
}

struct Tracer(Arc<Mutex<Trace>>);

impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self.0.lock().unwrap();
        trace
            .names
            .insert(event.function_id, event.function_name.into());
        if trace.recording {
            trace.count += 1;
            if trace.ticks.len() < 256 {
                trace.ticks.push((event.function_id, event.op));
            }
        }
    }
}

struct Harness {
    runtime: Runtime,
    oracle: Runtime,
    trace: Arc<Mutex<Trace>>,
    artifacts: JitArtifactBatch,
    events: Vec<JitDebugEvent>,
}

impl Harness {
    fn new(setup: &str) -> Self {
        Self::new_with_selection(setup, JitSelection::Template)
    }

    fn new_with_selection(setup: &str, selection: JitSelection) -> Self {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .build()
            .unwrap();
        runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
        let mut output = runtime
            .run_script(
                SourceInput::from_javascript(setup),
                "template-direct-setup.js",
            )
            .unwrap();
        let artifacts = output.take_jit_artifacts().unwrap();
        assert!(!artifacts.truncated());
        let events = output.jit_debug_report().unwrap().events().to_vec();
        let mut oracle = Runtime::builder()
            .jit_selection(JitSelection::InterpreterOnly)
            .build()
            .unwrap();
        oracle
            .run_script(
                SourceInput::from_javascript(setup),
                "template-direct-setup.js",
            )
            .unwrap();
        Self {
            runtime,
            oracle,
            trace,
            artifacts,
            events,
        }
    }

    fn fid(&self, name: &str) -> u32 {
        *self
            .trace
            .lock()
            .unwrap()
            .names
            .iter()
            .find(|(_, observed)| observed.as_str() == name)
            .unwrap_or_else(|| panic!("warm dispatch did not identify {name}"))
            .0
    }

    fn current(&self, name: &str) -> JitCodeGenerationSnapshot {
        let fid = self.fid(name);
        let current: Vec<_> = self
            .runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .filter(|entry| {
                entry.function_id == fid
                    && entry.tier == NativeFrameKind::Baseline
                    && entry.lifecycle == CodeLifetimeState::Installed
                    && entry.linked
            })
            .collect();
        assert_eq!(current.len(), 1, "current own Template entry for {name}");
        current.into_iter().next().unwrap()
    }

    fn assert_edge(
        &self,
        caller: &JitCodeGenerationSnapshot,
        callee: &JitCodeGenerationSnapshot,
        kind: JitDirectCallKind,
        target_count: u32,
    ) {
        assert!(
            self.events.iter().any(|event| matches!(event,
                JitDebugEvent::DirectCallLowered {
                    caller_code_object_id, callee_function_id, call_kind, target_count: count,
                    outcome: JitDirectCallLoweringOutcome::Generated { code_object_id, .. }, ..
                } if *caller_code_object_id == caller.code_object_id
                    && *callee_function_id == callee.function_id && *call_kind == kind
                    && *code_object_id == callee.code_object_id && *count == target_count
            )),
            "exact installed caller owns this generated target"
        );
        let bundle = self
            .artifacts
            .bundles()
            .iter()
            .find(|bundle| bundle.manifest().code_object_id() == caller.code_object_id)
            .unwrap();
        assert_eq!(bundle.manifest().entry(), JitDebugTarget::Entry);
        let relocations: serde_json::Value = serde_json::from_slice(
            bundle
                .file(JitArtifactFileName::Relocations)
                .unwrap()
                .contents(),
        )
        .unwrap();
        let links: Vec<_> = relocations["relocations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| {
                entry["target"]["kind"] == "functionEntryCell"
                    && entry["target"]["functionId"].as_u64() == Some(u64::from(callee.function_id))
            })
            .collect();
        assert_eq!(links.len(), 1, "one current-generation native link");
        let end = links[0]["endOffset"].as_u64().unwrap() as usize;
        let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
        #[cfg(target_arch = "aarch64")]
        assert_eq!(
            &code[end..end + 12],
            &[
                0x08, 0x01, 0x40, 0xf9, 0x10, 0x01, 0x40, 0xf9, 0x00, 0x02, 0x3f, 0xd6
            ]
        );
        #[cfg(target_arch = "x86_64")]
        assert_eq!(&code[end..end + 6], &[0x4d, 0x8b, 0x0b, 0x41, 0xff, 0x11]);
        return_sites::assert_known(bundle, callee.function_id);
    }

    fn probe(&mut self, source: &str, expected: &str, native: &[&str]) {
        {
            let mut trace = self.trace.lock().unwrap();
            trace.count = 0;
            trace.ticks.clear();
            trace.recording = true;
        }
        let output = self
            .runtime
            .run_script(
                SourceInput::from_javascript(source),
                "template-direct-probe.js",
            )
            .unwrap();
        let oracle = self
            .oracle
            .run_script(
                SourceInput::from_javascript(source),
                "template-direct-probe.js",
            )
            .unwrap();
        assert_eq!(oracle.completion_string(), expected);
        assert_eq!(output.completion_string(), oracle.completion_string());
        let trace = self.trace.lock().unwrap();
        assert_eq!(
            trace.count,
            trace.ticks.len(),
            "complete isolated probe trace"
        );
        assert!(
            trace
                .ticks
                .iter()
                .any(|(fid, op)| trace.names[fid] == "<main>"
                    && matches!(op, Op::Return | Op::ReturnUndefined | Op::ReturnValue))
        );
        for name in native {
            assert!(
                !trace.ticks.iter().any(|(fid, _)| trace.names[fid] == *name),
                "{name} must execute natively"
            );
        }
    }
}

const DICTIONARY_METHOD_SETUP: &str = r#"
const dictionaryPrototype = Object.create(null);
dictionaryPrototype.method = function dictionaryMethod(value) { return this.marker + value; };
for (let index = 0; index < 129; index++) { dictionaryPrototype['padding' + index] = index; }
const dictionaryReceiver = Object.create(dictionaryPrototype);
dictionaryReceiver.marker = 8;
function inheritedDictionaryMethodSite() { return dictionaryReceiver.method(41); }
"#;

#[test]
fn wide_dictionary_inherited_method_preserves_first_site_admission_semantics() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        // Each selected runtime installs its JIT hook; the harness also runs
        // the same untouched setup and first method probe in InterpreterOnly.
        let mut harness = Harness::new_with_selection(DICTIONARY_METHOD_SETUP, selection);
        harness.probe(
            "JSON.stringify([inheritedDictionaryMethodSite(), inheritedDictionaryMethodSite(), Object.keys(dictionaryPrototype).length, Object.prototype.hasOwnProperty.call(dictionaryReceiver, 'method'), Object.getPrototypeOf(dictionaryReceiver) === dictionaryPrototype]);",
            "[49,49,130,false,true]",
            &[],
        );
    }
}

const METHOD_SETUP: &str = r#"
function methodIncrease(v) { const a = new Array(); return this.n + v + a.length; }
function methodDecrease(v) { const a = new Array(); return this.n - v - a.length; }
const firstPrototype = {step: methodIncrease};
const secondPrototype = {step: methodDecrease};
const firstReceiver = Object.create(firstPrototype); firstReceiver.n = 100;
const secondReceiver = Object.create(secondPrototype); secondReceiver.n = 200; secondReceiver.extra = 1;
function invokeMethod(o, v) { return o.step(v); }
for (let warm = 0; warm < 1000; warm++) { methodIncrease.call(firstReceiver, 1); methodDecrease.call(secondReceiver, 1); }
for (let warm = 0; warm < 20000; warm++) { invokeMethod(firstReceiver, 1); invokeMethod(secondReceiver, 1); }
let getterReads = 0;
function getReplacement() { getterReads++; return methodDecrease; }
"#;

#[test]
fn polymorphic_inherited_methods_enter_current_native_targets_and_commit_miss_once() {
    let mut harness = Harness::new(METHOD_SETUP);
    let caller = harness.current("invokeMethod");
    let increase = harness.current("methodIncrease");
    let decrease = harness.current("methodDecrease");
    harness.assert_edge(&caller, &increase, JitDirectCallKind::Method, 2);
    harness.assert_edge(&caller, &decrease, JitDirectCallKind::Method, 2);
    harness.probe(
        "JSON.stringify([invokeMethod(firstReceiver, 3), invokeMethod(secondReceiver, 2)]);",
        "[103,198]",
        &["invokeMethod", "methodIncrease", "methodDecrease"],
    );
    for (name, before) in [("methodIncrease", increase), ("methodDecrease", decrease)] {
        let after = harness.current(name);
        assert_eq!(after.code_object_id, before.code_object_id);
        assert_eq!(after.generated_entries, before.generated_entries + 1);
        assert_eq!(after.generated_deopts, before.generated_deopts);
    }
    assert_eq!(
        harness.current("invokeMethod").code_object_id,
        caller.code_object_id
    );
    // A fresh receiver leaves the warmed prototype dependency intact. The
    // existing native caller must miss its shape chain and commit the getter
    // resolution once; mutating the original prototype before entry could
    // instead invalidate the caller and test only interpreter semantics.
    harness.probe("const getterReceiver = {n: 300}; Object.defineProperty(getterReceiver, 'step', {get: getReplacement}); JSON.stringify([invokeMethod(getterReceiver, 3), getterReads]);",
        "[297,1]", &["invokeMethod"]);
    let exited = harness
        .runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .find(|entry| entry.code_object_id == caller.code_object_id)
        .unwrap();
    assert_eq!(
        exited.generated_deopts, caller.generated_deopts,
        "committed method resolution never deopts and replays the getter"
    );
    harness.probe("getterReads = 0; Object.defineProperty(firstPrototype, 'step', {get: getReplacement}); JSON.stringify([invokeMethod(firstReceiver, 3), getterReads]);",
        "[97,1]", &[]);
}

const CONSTRUCTOR_SETUP: &str = r#"
function DirectConstructor(v, absentA, absentB) {
    this.value = v;
    this.array = new Array();
    this.correctTarget = new.target === DirectConstructor;
    this.missing = absentA === undefined && absentB === undefined;
}
function makeDirect(Ctor, v) { return new Ctor(v); }
for (let warm = 0; warm < 20000; warm++) { makeDirect(DirectConstructor, 1); }
"#;

#[test]
fn known_constructor_preserves_new_target_receiver_prototype_and_missing_actuals() {
    let mut harness = Harness::new(CONSTRUCTOR_SETUP);
    let caller = harness.current("makeDirect");
    let callee = harness.current("DirectConstructor");
    harness.assert_edge(&caller, &callee, JitDirectCallKind::Construct, 1);
    harness.probe("const madeFirst = makeDirect(DirectConstructor, 10); const madeSecond = makeDirect(DirectConstructor, 20); JSON.stringify([madeFirst.value, madeSecond.value, madeFirst.correctTarget && madeSecond.correctTarget, madeFirst.missing && madeSecond.missing, madeFirst instanceof DirectConstructor && madeSecond instanceof DirectConstructor, madeFirst.array.length + madeSecond.array.length]);",
        "[10,20,true,true,true,0]", &["makeDirect", "DirectConstructor"]);
    let after = harness.current("DirectConstructor");
    assert_eq!(after.code_object_id, callee.code_object_id);
    assert_eq!(after.generated_entries, callee.generated_entries + 2);
    assert_eq!(after.generated_deopts, callee.generated_deopts);
    assert_eq!(
        harness.current("makeDirect").code_object_id,
        caller.code_object_id
    );
}
