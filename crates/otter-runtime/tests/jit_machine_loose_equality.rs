//! Machine IR tagged/nullish loose-equality coverage.
//!
//! # Contents
//! - Direct null/undefined, Number, and Boolean comparison semantics.
//! - Exact ordinary-object deoptimization without coercion or replay.
//! - Machine code-map, deopt, relocation, and empty-safepoint proofs.
//!
//! # Invariants
//! - A static nullish operand forms one equivalence class containing exactly
//!   `null` and `undefined` for every non-cell JavaScript value.
//! - Every cell exits at the original comparison before the Boolean result is
//!   defined, so the canonical path retains the HTMLDDA decision.
//! - The generated comparison neither allocates nor owns a safepoint.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTier,
    JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";
const FUNCTION_NAMES: [(&str, bool); 4] = [
    ("machineLooseEqNull", true),
    ("machineLooseNeNull", false),
    ("machineLooseEqUndefined", true),
    ("machineLooseNeUndefined", false),
];

const SETUP: &str = r#"
function machineLooseEqNull(value) {
  return value == null;
}

function machineLooseNeNull(value) {
  return value != null;
}

function machineLooseEqUndefined(value) {
  return undefined == value;
}

function machineLooseNeUndefined(value) {
  return undefined != value;
}

for (let warm = 0; warm < 5000; warm++) {
  let value;
  switch (warm & 3) {
    case 0: value = null; break;
    case 1: value = undefined; break;
    case 2: value = warm | 0; break;
    default: value = (warm & 4) !== 0; break;
  }
  machineLooseEqNull(value);
  machineLooseNeNull(value);
  machineLooseEqUndefined(value);
  machineLooseNeUndefined(value);
}
"#;

const PRIMITIVE_PROBE: &str = r#"
JSON.stringify([
  [machineLooseEqNull(null), machineLooseNeNull(null), machineLooseEqUndefined(null), machineLooseNeUndefined(null)],
  [machineLooseEqNull(undefined), machineLooseNeNull(undefined), machineLooseEqUndefined(undefined), machineLooseNeUndefined(undefined)],
  [machineLooseEqNull(17), machineLooseNeNull(17), machineLooseEqUndefined(17), machineLooseNeUndefined(17)],
  [machineLooseEqNull(false), machineLooseNeNull(false), machineLooseEqUndefined(false), machineLooseNeUndefined(false)],
  [machineLooseEqNull(true), machineLooseNeNull(true), machineLooseEqUndefined(true), machineLooseNeUndefined(true)]
]);
"#;

const OBJECT_PROBE: &str = r#"
let machineLooseCoercions = 0;
let machineLooseCaught = 0;
const machineLooseObject = {
  [Symbol.toPrimitive]() {
    machineLooseCoercions++;
    throw new Error("nullish comparison coerced an ordinary object");
  }
};
let machineLooseObjectResults;
try {
  machineLooseObjectResults = [
    machineLooseEqNull(machineLooseObject),
    machineLooseNeNull(machineLooseObject),
    machineLooseEqUndefined(machineLooseObject),
    machineLooseNeUndefined(machineLooseObject)
  ];
} catch (_) {
  machineLooseCaught++;
}
JSON.stringify([machineLooseObjectResults, machineLooseCoercions, machineLooseCaught]);
"#;

fn runtime(selection: JitSelection, artifacts: bool) -> Runtime {
    let builder = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX);
    if artifacts {
        builder.jit_debug(JitDebugRequest::artifacts()).build()
    } else {
        builder.build()
    }
    .expect("Machine loose-equality runtime")
}

fn completion(runtime: &mut Runtime, source: &str, module: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), module)
        .unwrap_or_else(|error| panic!("Machine loose-equality fixture {module}: {error:?}"))
        .completion_string()
        .to_owned()
}

fn artifact_json(bundle: &JitArtifactBundle, file: JitArtifactFileName) -> serde_json::Value {
    serde_json::from_slice(
        bundle
            .file(file)
            .unwrap_or_else(|| panic!("missing {file:?} in {:?}", bundle.manifest()))
            .contents(),
    )
    .unwrap_or_else(|error| panic!("invalid {file:?} JSON: {error}"))
}

fn machine_bundle<'a>(
    artifacts: &'a JitArtifactBatch,
    function_name: &str,
) -> &'a JitArtifactBundle {
    artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == "jit-machine-loose-equality-setup.js"
                && manifest.function_name() == function_name
                && manifest.tier() == JitDebugTier::Optimizing
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
                && bundle
                    .file(JitArtifactFileName::CodeMap)
                    .is_some_and(|file| {
                        serde_json::from_slice::<serde_json::Value>(file.contents()).is_ok_and(
                            |code_map| {
                                code_map["regions"].as_array().is_some_and(|regions| {
                                    regions
                                        .iter()
                                        .any(|region| region["kind"] == "machineTaggedNullishEqual")
                                })
                            },
                        )
                    })
        })
        .unwrap_or_else(|| {
            let manifests = artifacts
                .bundles()
                .iter()
                .map(|bundle| {
                    let manifest = bundle.manifest();
                    format!(
                        "{}:{}:{:?}",
                        manifest.module(),
                        manifest.function_name(),
                        manifest.tier()
                    )
                })
                .collect::<Vec<_>>();
            panic!("missing {function_name} Machine loose-equality bundle: {manifests:?}")
        })
}

fn assert_machine_artifact(bundle: &JitArtifactBundle, equal: bool) {
    let optimized_ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("Machine loose-equality IR")
            .contents(),
    )
    .expect("UTF-8 Machine loose-equality IR");
    assert!(
        optimized_ir.contains(&format!("equal: {equal}")),
        "Machine IR must preserve == versus !=: {optimized_ir}"
    );
    assert!(
        optimized_ir.contains("TaggedNullishEqual"),
        "Machine IR must select the tagged/nullish opcode: {optimized_ir}"
    );

    let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let matching = code_map["regions"]
        .as_array()
        .expect("code-map regions")
        .iter()
        .filter(|region| region["kind"] == "machineTaggedNullishEqual")
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "one tagged/nullish region: {code_map}");
    let byte_pc = matching[0]["bytePc"]
        .as_u64()
        .expect("tagged/nullish byte PC");
    assert!(
        matching[0]["endOffset"].as_u64() > matching[0]["startOffset"].as_u64(),
        "the generated comparison must have non-zero native width: {code_map}"
    );

    let deopt = artifact_json(bundle, JitArtifactFileName::Deopt);
    assert!(
        deopt["exits"]
            .as_array()
            .expect("deopt exits")
            .iter()
            .flat_map(|exit| exit["frames"].as_array().into_iter().flatten())
            .any(|frame| frame["bytePc"].as_u64() == Some(byte_pc)),
        "the cell guard must reconstruct the exact comparison PC: {deopt}"
    );

    let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
    let relocations = relocations["relocations"]
        .as_array()
        .expect("relocation entries");
    assert!(
        relocations.iter().all(|relocation| {
            relocation["target"]["kind"] != "runtimeStub"
                || relocation["target"]["name"] == "jit_deopt_rebuild_frames"
        }),
        "tagged/nullish emission may call only the shared exact-deopt handler: {relocations:?}"
    );

    let safepoints = artifact_json(bundle, JitArtifactFileName::Safepoints);
    assert_eq!(
        safepoints["safepoints"].as_array().map(Vec::len),
        Some(0),
        "the comparison allocates, reenters, and safepoints nowhere"
    );
}

fn stats_delta(before: RuntimeExecutionStats, after: RuntimeExecutionStats) -> (u64, u64) {
    (
        after.jit_optimized_entries - before.jit_optimized_entries,
        after.jit_optimized_deopts - before.jit_optimized_deopts,
    )
}

fn compiled_fixture(artifacts: bool) -> Runtime {
    let mut runtime = runtime(JitSelection::ProductionTiered, artifacts);
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(SETUP),
            "jit-machine-loose-equality-setup.js",
        )
        .expect("Machine loose-equality setup");
    if artifacts {
        let artifacts = setup
            .jit_artifacts()
            .expect("Machine loose-equality artifacts");
        for (function_name, equal) in FUNCTION_NAMES {
            assert_machine_artifact(machine_bundle(artifacts, function_name), equal);
        }
    }
    drop(setup);
    runtime
}

#[test]
fn machine_tagged_nullish_primitives_match_interpreter_without_deopt() {
    let mut oracle = runtime(JitSelection::InterpreterOnly, false);
    completion(
        &mut oracle,
        SETUP,
        "jit-machine-loose-equality-oracle-setup.js",
    );
    let expected = completion(
        &mut oracle,
        PRIMITIVE_PROBE,
        "jit-machine-loose-equality-oracle-primitives.js",
    );
    assert_eq!(
        expected,
        "[[true,false,true,false],[true,false,true,false],[false,true,false,true],[false,true,false,true],[false,true,false,true]]"
    );

    let mut compiled = compiled_fixture(true);
    let before = compiled.execution_stats();
    let actual = completion(
        &mut compiled,
        PRIMITIVE_PROBE,
        "jit-machine-loose-equality-primitives.js",
    );
    let (entries, deopts) = stats_delta(before, compiled.execution_stats());
    assert_eq!(actual, expected);
    assert!(
        entries >= 20,
        "every primitive probe must enter Machine code"
    );
    assert_eq!(deopts, 0, "non-cell values complete in generated code");
}

#[test]
fn machine_tagged_nullish_cells_exact_deopt_without_coercion_or_replay() {
    let mut compiled = compiled_fixture(false);
    let before = compiled.execution_stats();
    let actual = completion(
        &mut compiled,
        OBJECT_PROBE,
        "jit-machine-loose-equality-object.js",
    );
    let after_object = compiled.execution_stats();
    let (entries, deopts) = stats_delta(before, after_object);
    assert_eq!(actual, "[[false,true,false,true],0,0]");
    assert!(entries >= 4, "each cell comparison must enter Machine code");
    assert_eq!(
        deopts, 4,
        "each function must exact-deopt once before its Boolean destination"
    );

    let reused = completion(
        &mut compiled,
        r#"JSON.stringify([
          machineLooseEqNull(null),
          machineLooseNeNull(1),
          machineLooseEqUndefined(undefined),
          machineLooseNeUndefined(false)
        ]);"#,
        "jit-machine-loose-equality-reuse.js",
    );
    let (reuse_entries, reuse_deopts) = stats_delta(after_object, compiled.execution_stats());
    assert_eq!(reused, "[true,true,true,true]");
    assert!(reuse_entries >= 4, "all generated bodies remain reusable");
    assert_eq!(reuse_deopts, 0, "primitive reuse stays on the fast path");
}
