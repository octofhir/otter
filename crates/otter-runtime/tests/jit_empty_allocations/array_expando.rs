//! Native array expando storage with original receiver reconstruction.
//!
//! # Contents
//! - Natural-policy own field reads and overwrites in default and added realms.
//! - Retained generations across full collection and accessor transitions.
//! - Evaluated RHS, receiver identity and committed accessor effects.
//!
//! # Invariants
//! - A tagged bag owns the field and its barrier; eager state keeps the array.
//! - A cold forwarding branch retains each subject's own entry.
//! - A descriptor miss resumes the source operation once with its original RHS.
//! - Full collection verifies retained roots; minor edges have separate VM tests.
//!
//! # See also
//! - `otter_vm::array::expando` owns attachment and traced storage.
//! - `otter_jit::graph::builder_field_tests` checks exact barrier ownership.

use super::*;

const MODULE: &str = "native-array-expando-setup.js";
const SETUP: &str = r#"
const expandoArray = [];
expandoArray.named = {marker: 7};
function expandoRead(receiver) {
  if (receiver.length < 0) return expandoRead.apply(null, arguments);
  return receiver.named;
}
function expandoWrite(receiver, value) {
  if (receiver.length < 0) return expandoWrite.apply(null, arguments);
  receiver.named = value;
  return receiver.named;
}
for (let warm = 0; warm < 10000; warm++) {
  expandoRead(expandoArray);
  expandoWrite(expandoArray, expandoArray.named);
}
"#;

fn probe(extra_realm: bool) {
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let realm = extra_realm.then(|| runtime.create_realm().unwrap());
    let setup = run(&mut runtime, realm, SETUP, MODULE);
    let batch = setup.jit_artifacts().expect("array field artifacts");
    for function in ["expandoRead", "expandoWrite"] {
        let own: Vec<_> = batch
            .bundles()
            .iter()
            .filter(|bundle| {
                bundle.manifest().module() == MODULE
                    && bundle.manifest().function_name() == function
                    && bundle.manifest().tier() == JitDebugTier::Optimizing
            })
            .collect();
        assert!(!own.is_empty(), "normal policy must admit {function}");
        assert!(
            own.iter().any(|bundle| {
                let ir = std::str::from_utf8(
                    bundle
                        .file(JitArtifactFileName::OptimizedIr)
                        .unwrap()
                        .contents(),
                )
                .unwrap();
                ir.contains("LoadArrayExpando(")
                    && ir.contains("LoadOwnField(")
                    && (function != "expandoWrite"
                        || (ir.contains("StoreOwnField") && ir.contains("WriteBarrier")))
            }),
            "own tagged bag field lowering for {function}"
        );
        for bundle in own {
            let map = json(bundle, JitArtifactFileName::CodeMap);
            assert!(
                map["regions"].as_array().unwrap().iter().any(|region| {
                    region["operation"]
                        .as_str()
                        .is_some_and(|operation| operation.contains("LoadArrayExpando"))
                }),
                "emitted bag projection belongs to {function}"
            );
        }
    }
    drop(setup);
    runtime.force_gc().expect("collect retained bag proofs");
    let value = run(
        &mut runtime,
        realm,
        r#"
let expandoGets = 0, expandoSets = 0, expandoRhs = 0;
let expandoGetReceiver, expandoSetReceiver;
let expandoYoung = {marker: 11};
const expandoPressure = [];
Object.defineProperty(expandoArray, "named", {
  configurable: true,
  get() {
    expandoGets++; expandoGetReceiver = this;
    expandoPressure.push({keep: expandoYoung, array: this});
    return expandoYoung;
  },
  set(value) { expandoSets++; expandoSetReceiver = this; expandoYoung = value; }
});
const expandoReadResult = expandoRead(expandoArray);
function expandoEvaluatedRhs() { expandoRhs++; return {marker: 13}; }
const expandoWriteResult = expandoWrite(expandoArray, expandoEvaluatedRhs());
JSON.stringify([expandoReadResult.marker, expandoWriteResult.marker,
 expandoGetReceiver === expandoArray, expandoSetReceiver === expandoArray,
 expandoGets, expandoSets, expandoRhs, expandoPressure.length,
 Object.getPrototypeOf(expandoYoung) === Object.prototype]);
"#,
        "native-array-expando-probe.js",
    );
    assert_eq!(value.completion_string(), "[11,13,true,true,2,1,1,2,true]");
    let report = value.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert!(
        report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::Bail { .. } | JitDebugEvent::EnteredGenerationDeopt { .. }
        )),
        "accessor transition must reach an original source exit: {report:?}"
    );
    drop(value);
    runtime
        .force_gc()
        .expect("collect committed accessor results");
    assert_eq!(run(&mut runtime, realm,
        "JSON.stringify([expandoRead(expandoArray).marker, expandoGets, expandoSets, expandoRhs])",
        "native-array-expando-reuse.js").completion_string(), "[13,3,1,1]");
}

#[test]
fn native_array_fields_keep_default_source_and_receiver() {
    probe(false);
}

#[test]
fn native_array_fields_keep_additional_source_and_receiver() {
    probe(true);
}
