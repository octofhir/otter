//! Reclamation of source retained transitively by compiled context constants.
//!
//! # Contents
//! - A static source's optimizing closure holding an eval closure through objects.
//! - Exact native admission, moving collection, and complete source reclamation.
//!
//! # Invariants
//! - The reader never calls or splices the eval function it returns.
//! - Dropping the reader changes no watched context slot or object property.
//! - Background compilation remains enabled; source leases settle before census.
//!
//! # See also
//! - `otter_vm::jit_roots` owns compiled constants through physical retirement.
//! - `otter_vm::code_liveness` reports live contexts' descriptor-owning sources.

use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitSelection, Runtime,
    SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{CodeDependencyKind, CodeLifetimeState, NativeFrameKind},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Trace {
    eval_leaf: Option<u32>,
    recording: bool,
    ids: Vec<u32>,
}

struct Tracer(Arc<Mutex<Trace>>);

impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self.0.lock().unwrap();
        if event.function_name == "evalLeaf" {
            if let Some(previous) = trace.eval_leaf {
                assert_eq!(previous, event.function_id);
            }
            trace.eval_leaf = Some(event.function_id);
        }
        if trace.recording {
            assert!(trace.ids.len() < 16_384, "complete bounded probe trace");
            trace.ids.push(event.function_id);
        }
    }
}

fn reader(
    runtime: &Runtime,
    artifacts: &BTreeMap<u64, JitArtifactBundle>,
) -> Option<JitCodeGenerationSnapshot> {
    runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .find(|generation| {
            generation.lifecycle == CodeLifetimeState::Installed
                && generation.linked
                && generation.current_entry
                && generation.tier == NativeFrameKind::Optimizing
                && artifacts
                    .get(&generation.code_object_id)
                    .is_some_and(|bundle| bundle.manifest().function_name() == "outsideReader")
        })
}

#[test]
fn dead_eval_source_is_released_from_an_unrelated_compiled_context_constant() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let trace = Arc::new(Mutex::new(Trace::default()));
    runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    let mut artifacts = BTreeMap::new();
    // A zero-formal reader has no mapped-parameter context store that shares
    // the captured box's slot coordinate and prevents constant admission.
    let definitions = r#"
function makeOutsideReader(value) {
  if (value() !== 41) throw "eval result";
  const box = {nested: {fn: value}};
  return function outsideReader() {
    if (arguments === 7) throw "own arguments";
    return box.nested.fn;
  };
}
globalThis.__outsideReader = makeOutsideReader(eval(`
  (() => {
    const seed = 41;
    return function evalLeaf() { return seed; };
  })()
`));
for (let warm = 0; warm < 4096; warm++) __outsideReader(7);
"ready";
"#;
    for batch in 0..=128 {
        let source = if batch == 0 {
            definitions.to_owned()
        } else {
            format!("{}\n\"ready\";", "__outsideReader(7);\n".repeat(32))
        };
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source),
                &format!("compiled-constant-admission-{batch}.js"),
            )
            .unwrap();
        assert_eq!(result.completion_string(), "ready");
        let captured = result.jit_artifacts().unwrap();
        assert!(!captured.truncated());
        for bundle in captured.bundles() {
            artifacts.insert(bundle.manifest().code_object_id(), bundle.clone());
        }
        if runtime.jit_code_residency().pending_compilations == 0
            && reader(&runtime, &artifacts).is_some()
        {
            break;
        }
        assert!(batch < 128, "normal-policy optimizing reader admission");
    }

    let before = reader(&runtime, &artifacts).unwrap();
    let eval_leaf = trace.lock().unwrap().eval_leaf.expect("executed eval leaf");
    assert_ne!(before.function_id, eval_leaf);
    let bundle = &artifacts[&before.code_object_id];
    let ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert!(
        ir.contains("LoadCellValue"),
        "compiled strong constant: {ir}"
    );
    // Final lowering replaces individual context proofs with dependency tests.
    // Inspect the exact installed dependencies rather than a pre-pass IR opcode.
    let dependencies = before.dependencies.as_ref().unwrap();
    assert!(dependencies.iter().any(|dependency| {
        dependency.kind == CodeDependencyKind::ClosureContext
            && dependency.identity == before.function_id
    }));
    assert!(
        dependencies
            .iter()
            .any(|dependency| dependency.kind == CodeDependencyKind::ContextSlot),
        "captured object constant: {dependencies:?}"
    );
    let relocations: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::Relocations)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert!(
        relocations["relocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|relocation| {
                relocation["target"]["kind"] == "calleeIdentityCell"
                    && relocation["target"]["functionId"].as_u64()
                        == Some(u64::from(before.function_id))
            }),
        "the captured slot value owns an emitted cell reference"
    );
    let map: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::CodeMap)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let instructions: Vec<_> = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| region["kind"] == "instruction")
        .collect();
    assert!(!instructions.is_empty());
    assert!(
        instructions
            .iter()
            .all(|region| { region["functionId"].as_u64() == Some(u64::from(before.function_id)) }),
        "the exact reader generation splices no eval body: {map}"
    );

    runtime.force_gc().unwrap();
    assert_eq!(
        reader(&runtime, &artifacts).unwrap().code_object_id,
        before.code_object_id
    );
    trace.lock().unwrap().recording = true;
    let probe = runtime
        .run_script(
            SourceInput::from_javascript("typeof __outsideReader(7);"),
            "compiled-constant-native-probe.js",
        )
        .unwrap();
    trace.lock().unwrap().recording = false;
    assert_eq!(probe.completion_string(), "function");
    let report = probe.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    assert!(
        !report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::Bail { .. }
                | JitDebugEvent::EnteredGenerationDeopt { .. }
                | JitDebugEvent::InlineDeoptFrame { .. }
        )),
        "{:?}",
        report.events()
    );
    {
        let trace = trace.lock().unwrap();
        assert!(!trace.ids.is_empty(), "the probe traced its script entry");
        assert!(
            !trace.ids.contains(&before.function_id),
            "reader stayed native"
        );
        assert!(
            !trace.ids.contains(&eval_leaf),
            "reader only returns the eval closure"
        );
    }
    assert_eq!(
        reader(&runtime, &artifacts).unwrap().code_object_id,
        before.code_object_id
    );

    runtime
        .run_script(
            SourceInput::from_javascript("globalThis.__outsideReader = undefined; \"dropped\";"),
            "compiled-constant-drop.js",
        )
        .unwrap();
    assert_eq!(runtime.jit_code_residency().pending_compilations, 0);
    assert_eq!(
        reader(&runtime, &artifacts).unwrap().code_object_id,
        before.code_object_id,
        "dropping the semantic owner does not fire the captured-slot proof"
    );
    let retained = runtime.code_eviction_stats().retained_bytes;
    assert!(
        retained > 0,
        "the eval payload is still linked before census"
    );
    let evicted = runtime.code_eviction_stats().evicted_bytes;
    runtime.set_code_eviction_high_water_bytes(0);
    runtime.reclaim_dynamic_code().unwrap();
    let after = runtime.code_eviction_stats();
    assert_eq!(
        after.retained_bytes, 0,
        "no compiled constant may retain dead source"
    );
    assert_eq!(after.evicted_bytes - evicted, retained);
    let retired = runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .find(|generation| generation.code_object_id == before.code_object_id)
        .unwrap();
    assert_eq!(retired.lifecycle, CodeLifetimeState::Retired);
    assert_eq!(retired.active_count, 0);
}
