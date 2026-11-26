//! Admission of a deeper Graph from calls executed in an installed root.
//!
//! # Contents
//! A native Array mapper first calls its callee through compact code and
//! subsequently earns an inlined generation under ordinary tier policy.
//!
//! # Invariants
//! Each getter runs exactly once. The object it returns remains reachable
//! across the call and the generation change. Both tiers compute the same value.
//!
//! # See also
//! `otter_vm::native_abi::SourceWork` owns the execution evidence.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use otter_runtime::{
    JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitSelection, Runtime, SourceInput,
};

const PROGRAM: &str = r#"
let count = 0, retained = {value: 6};
const holder = {get input() { ++count; return retained; }};
function budgetLeaf(obj) { return obj.value + 1; }
function budgetCaller(i) { const obj = holder.input; return budgetLeaf(obj) + i; }
const values = new Array(512).fill(3);
let result = 0;
for (let i = 0; i < 72; i++) result += values.map(budgetCaller)[511];
JSON.stringify([result, count, retained.value]);
"#;

#[test]
fn executed_calls_expand_the_root_and_preserve_getter_effects() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_compile_threads(0)
            .jit_debug(JitDebugRequest::events().with_artifacts(true))
            .build()
            .unwrap();
        let result = runtime
            .run_script(SourceInput::from_javascript(PROGRAM), "inline-admission.js")
            .unwrap();
        assert_eq!(result.completion_string(), "[720,36864,6]");
        if selection == JitSelection::InterpreterOnly {
            continue;
        }
        let batch = result.jit_artifacts().unwrap();
        let compact = batch
            .bundles()
            .iter()
            .filter(|bundle| bundle.manifest().function_name() == "budgetCaller")
            .find(|bundle| {
                bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| {
                        std::str::from_utf8(file.contents())
                            .unwrap()
                            .contains("CountInlineCall")
                    })
            })
            .expect("caller metered an eligible call");
        let caller = compact.manifest().function_id();
        let first = compact.manifest().code_object_id();
        let leaf = batch
            .bundles()
            .iter()
            .find(|bundle| bundle.manifest().function_name() == "budgetLeaf")
            .expect("leaf has an independent generation")
            .manifest()
            .function_id();
        let current = runtime
            .jit_code_generation_snapshot()
            .into_iter()
            .find(|generation| generation.function_id == caller && generation.current_entry)
            .expect("caller has a current native entry");
        assert!(current.linked && current.code_object_id > first);
        assert!(
            runtime
                .jit_code_generation_snapshot()
                .iter()
                .all(|generation| generation.code_object_id != first || !generation.linked),
            "a replaced compact mapping cannot remain eligible for new calls"
        );
        assert!(result.jit_debug_report().unwrap().events().iter().any(|event|
            matches!(event, JitDebugEvent::InlineLowered {code_object_id, parent_function_id, callee_function_id, ..}
                     if *code_object_id == current.code_object_id && *parent_function_id == caller && *callee_function_id == leaf)),
            "the current caller generation must splice its own leaf");
        let probe = runtime
            .run_script(
                SourceInput::from_javascript(
                    "JSON.stringify([budgetCaller(3),count,retained.value])",
                ),
                "inline-admission-probe.js",
            )
            .unwrap();
        assert_eq!(probe.completion_string(), "[10,36865,6]");
        assert!(
            runtime
                .jit_code_generation_snapshot()
                .iter()
                .any(|generation| generation.function_id == caller
                    && generation.current_entry
                    && generation.code_object_id == current.code_object_id)
        );
    }
}
