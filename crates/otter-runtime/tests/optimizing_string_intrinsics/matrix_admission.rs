//! Exact native admission for the primitive-string semantic matrix.
//!
//! # Contents
//! - Normal background admission of the three current Graph generations.
//! - Emitted string operations, interpreter-free probes and exact deopt joins.
//!
//! # Invariants
//! - Ordinary compiled calls are proven by their retained code and exits;
//!   materialized VM/OSR entry counters do not count this calling convention.
//! - The original coercion, replacement and reuse matrix remains unchanged.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitSelection, Runtime,
    SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{CodeLifetimeState, NativeFrameKind},
};

const NAMES: [&str; 3] = ["codeAt", "find", "findLiteral"];

struct Tracer(Arc<Mutex<Vec<u32>>>);

impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        self.0.lock().unwrap().push(event.function_id);
    }
}

pub(super) fn run_oracle(warmup: &str, matrix: &str) -> String {
    Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .unwrap()
        .run_script(
            SourceInput::from_javascript(format!("{warmup}\n{matrix}")),
            "optimizing-string-oracle.js",
        )
        .unwrap()
        .completion_string()
        .to_owned()
}

fn current(
    runtime: &Runtime,
    artifacts: &BTreeMap<u64, JitArtifactBundle>,
    name: &str,
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
                    .is_some_and(|bundle| bundle.manifest().function_name() == name)
        })
}

fn assert_exact_exit(events: &[JitDebugEvent], generation: &JitCodeGenerationSnapshot) {
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event,
                JitDebugEvent::EnteredGenerationDeopt {
                    callee_function_id,
                    callee_code_object_id,
                    callee_tier,
                    ..
                } if *callee_function_id == generation.function_id
                    && *callee_code_object_id == generation.code_object_id
                    && *callee_tier == otter_runtime::JitDebugTier::Optimizing
            ))
            .count(),
        1,
        "one actual native exit from the admitted generation: {events:?}"
    );
}

pub(super) fn run_native(warmup: &str, matrix: &str) -> String {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let mut artifacts = BTreeMap::new();
    for batch in 0..=128 {
        let source = if batch == 0 {
            warmup.to_owned()
        } else {
            "codeAt(\"alpha\", 0); find(\"alpha\", \"p\"); findLiteral(\"alpha\");\n".repeat(32)
        };
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source),
                &format!("optimizing-string-admission-{batch}.js"),
            )
            .unwrap();
        let captured = result.jit_artifacts().unwrap();
        assert!(!captured.truncated());
        for bundle in captured.bundles() {
            artifacts.insert(bundle.manifest().code_object_id(), bundle.clone());
        }
        if runtime.jit_code_residency().pending_compilations == 0
            && NAMES
                .iter()
                .all(|name| current(&runtime, &artifacts, name).is_some())
        {
            break;
        }
        assert!(batch < 128, "ordinary background Graph admission");
    }
    let originals = NAMES
        .iter()
        .map(|name| current(&runtime, &artifacts, name).unwrap())
        .collect::<Vec<_>>();
    for (index, generation) in originals.iter().enumerate() {
        assert!(generation.call_entry_offset.is_some());
        let bundle = &artifacts[&generation.code_object_id];
        let map: serde_json::Value = serde_json::from_slice(
            bundle
                .file(JitArtifactFileName::CodeMap)
                .unwrap()
                .contents(),
        )
        .unwrap();
        let operation = if index == 0 {
            " StringCharCodeAt "
        } else {
            " NativeLeaf("
        };
        assert!(
            map["regions"].as_array().unwrap().iter().any(|region| {
                region["kind"] == "instruction"
                    && region["functionId"].as_u64() == Some(u64::from(generation.function_id))
                    && region["operation"]
                        .as_str()
                        .is_some_and(|value| value.contains(operation))
                    && region["endOffset"].as_u64() > region["startOffset"].as_u64()
            }),
            "the exact native string operation was emitted: {map}"
        );
    }

    let trace = Arc::new(Mutex::new(Vec::new()));
    runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    let probe = runtime
        .run_script(
            SourceInput::from_javascript(
                "JSON.stringify([codeAt(\"alpha\", 0), find(\"alpha\", \"p\"), findLiteral(\"alpha\")]);",
            ),
            "optimizing-string-native-probe.js",
        )
        .unwrap();
    runtime.set_tracer(None);
    assert_eq!(probe.completion_string(), "[97,2,2]");
    let report = probe.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    assert!(report.events().is_empty());
    let ids = trace.lock().unwrap();
    assert!(!ids.is_empty(), "the cold script entry is traced");
    for (name, original) in NAMES.iter().zip(&originals) {
        assert!(
            !ids.contains(&original.function_id),
            "{name} dispatches no interpreter operations"
        );
        assert_eq!(
            current(&runtime, &artifacts, name).unwrap().code_object_id,
            original.code_object_id
        );
    }

    let result = runtime
        .run_script(
            SourceInput::from_javascript(matrix),
            "optimizing-string-matrix.js",
        )
        .unwrap();
    let report = result.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    // The fallback matrix actually exits the exact Graph generations selected
    // before the probe; successful compilation alone cannot satisfy this test.
    assert_exact_exit(report.events(), &originals[0]);
    assert_exact_exit(report.events(), &originals[1]);
    assert_eq!(
        current(&runtime, &artifacts, "findLiteral")
            .unwrap()
            .code_object_id,
        originals[2].code_object_id
    );

    // Also prove the literal caller's actual native entry with a guard miss.
    // The original matrix and its one coercion have already completed.
    let literal_miss = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
        let literalReplacementCalls = 0;
        const savedLiteralIndexOf = String.prototype.indexOf;
        String.prototype.indexOf = function() { literalReplacementCalls++; return 902; };
        const literalReplacementResult = findLiteral("alpha");
        String.prototype.indexOf = savedLiteralIndexOf;
        JSON.stringify([literalReplacementResult, literalReplacementCalls, findLiteral("alpha")]);
    "#,
            ),
            "optimizing-string-literal-replacement.js",
        )
        .unwrap();
    assert_eq!(literal_miss.completion_string(), "[902,1,2]");
    let report = literal_miss.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    assert_exact_exit(report.events(), &originals[2]);
    result.completion_string().to_owned()
}
