//! Machine IR direct global-load coverage.
//!
//! # Contents
//! - Live global-declarative cell and guarded global-object reads.
//! - Exact global-object epoch and shape misses with reusable execution.
//! - Machine code-map, relocation, deopt, and empty-safepoint proofs.
//!
//! # Invariants
//! - A prepared global load reads the live tagged slot without a runtime call.
//! - Lexical TDZ, global-declarative epoch, and global-object shape misses branch
//!   to the source operation's exact deopt before publishing an effect.
//! - Artifacts name the lexical cell symbolically and never serialize its
//!   process address.

#![cfg(target_arch = "aarch64")]

use std::collections::BTreeSet;

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTier,
    JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";

const SETUP: &str = r#"
let machineGlobalLexical = 40;
globalThis.machineGlobalObject = 2;

function machineGlobalLoads(bias) {
  return machineGlobalLexical + machineGlobalObject + bias;
}

for (let warm = 0; warm < 5000; warm++) {
  machineGlobalLoads(warm & 7);
}
"#;

fn runtime(selection: JitSelection, artifacts: bool) -> Runtime {
    let builder = Runtime::builder().jit_selection(selection);
    if artifacts {
        builder.jit_debug(JitDebugRequest::artifacts()).build()
    } else {
        builder.build()
    }
    .expect("Machine global-load runtime")
}

fn completion(runtime: &mut Runtime, source: &str, module: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), module)
        .unwrap_or_else(|error| panic!("Machine global-load fixture {module}: {error:?}"))
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

fn machine_global_bundle(artifacts: &JitArtifactBatch) -> &JitArtifactBundle {
    artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == "jit-machine-globals-setup.js"
                && manifest.function_name() == "machineGlobalLoads"
                && manifest.tier() == JitDebugTier::Optimizing
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
                && bundle
                    .file(JitArtifactFileName::CodeMap)
                    .is_some_and(|file| {
                        serde_json::from_slice::<serde_json::Value>(file.contents()).is_ok_and(
                            |code_map| {
                                let Some(regions) = code_map["regions"].as_array() else {
                                    return false;
                                };
                                [
                                    "machineBindingGuard",
                                    "machineBindingHit",
                                    "machineBindingCold",
                                ]
                                .into_iter()
                                .all(|kind| regions.iter().any(|region| region["kind"] == kind))
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
            panic!("missing exact Machine global-load bundle: {manifests:?}")
        })
}

fn assert_machine_global_artifact(bundle: &JitArtifactBundle) {
    // Each global read is one schema-driven Machine binding: a guard on the
    // live cell (lexical) or on the global object's shape and declarative
    // epoch (object), a hit that reads the slot, a cold path that completes
    // through the binding runtime when the guard fails, and the join.
    let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let regions = code_map["regions"].as_array().expect("code-map regions");
    let mut global_byte_pcs = BTreeSet::new();
    for kind in [
        "machineBindingGuard",
        "machineBindingHit",
        "machineBindingCold",
        "machineBindingJoin",
    ] {
        let matching = regions
            .iter()
            .filter(|region| region["kind"] == kind)
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 2, "one {kind} per global read: {code_map}");
        for region in matching {
            global_byte_pcs.insert(region["bytePc"].as_u64().expect("global-load byte PC"));
        }
    }
    assert_eq!(
        global_byte_pcs.len(),
        2,
        "two distinct global-read sites: {code_map}"
    );
    let cold_regions = regions
        .iter()
        .filter(|region| region["kind"] == "machineBindingCold")
        .count();

    let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
    let relocations = relocations["relocations"]
        .as_array()
        .expect("relocation entries");
    let lexical_cells = relocations
        .iter()
        .filter(|relocation| relocation["target"]["kind"] == "globalLexicalCell")
        .collect::<Vec<_>>();
    assert_eq!(lexical_cells.len(), 1, "one symbolic lexical cell");
    assert!(
        global_byte_pcs.contains(
            &lexical_cells[0]["target"]["bytePc"]
                .as_u64()
                .expect("lexical relocation byte PC")
        ),
        "lexical relocation must join its Machine region"
    );
    assert_eq!(
        relocations
            .iter()
            .filter(|relocation| relocation["target"]["kind"] == "gcCageBase")
            .count(),
        1,
        "the global-object guard needs one symbolic cage base"
    );
    assert!(
        relocations.iter().all(|relocation| {
            relocation["target"]["kind"] != "propertySourceCell"
                && (relocation["target"]["kind"] != "runtimeStub"
                    || relocation["target"]["name"] == "jit_deopt_rebuild_frames"
                    || relocation["target"]["name"] == "jit_finish_error"
                    || relocation["target"]["name"] == "jit_binding_value")
        }),
        "global reads may target only the binding runtime, the exact-deopt handler, \
         and the abrupt-completion finisher: {relocations:?}"
    );
    assert!(
        relocations.iter().any(|relocation| {
            relocation["target"]["kind"] == "runtimeStub"
                && relocation["target"]["name"] == "jit_binding_value"
        }),
        "the cold binding path must link the binding runtime: {relocations:?}"
    );

    let deopt = artifact_json(bundle, JitArtifactFileName::Deopt);
    let deopt_byte_pcs = deopt["exits"]
        .as_array()
        .expect("deopt exits")
        .iter()
        .flat_map(|exit| exit["frames"].as_array().into_iter().flatten())
        .filter_map(|frame| frame["bytePc"].as_u64())
        .collect::<BTreeSet<_>>();
    // A failed guard completes through the cold binding call and never
    // deoptimizes, so no exit is attributed to a global-read site.
    assert!(
        global_byte_pcs.is_disjoint(&deopt_byte_pcs),
        "global guards miss to the cold binding path instead of deopting: {deopt}"
    );

    let safepoints = artifact_json(bundle, JitArtifactFileName::Safepoints);
    assert_eq!(
        safepoints["safepoints"].as_array().map(Vec::len),
        Some(cold_regions),
        "only the cold binding calls reenter and safepoint"
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
            "jit-machine-globals-setup.js",
        )
        .expect("Machine global-load setup");
    if artifacts {
        assert_machine_global_artifact(machine_global_bundle(
            setup.jit_artifacts().expect("global-load artifacts"),
        ));
    }
    drop(setup);
    runtime
}

#[test]
fn machine_global_loads_read_live_slots_without_deopt_or_safepoint() {
    let mut oracle = runtime(JitSelection::InterpreterOnly, false);
    completion(&mut oracle, SETUP, "jit-machine-globals-oracle-setup.js");
    let expected = completion(
        &mut oracle,
        r#"
machineGlobalLexical = 50;
machineGlobalObject = 3;
JSON.stringify([machineGlobalLoads(0), machineGlobalLoads(1)]);
"#,
        "jit-machine-globals-oracle-probe.js",
    );
    assert_eq!(expected, "[53,54]");

    let mut compiled = compiled_fixture(true);
    let before = compiled.execution_stats();
    let actual = completion(
        &mut compiled,
        r#"
machineGlobalLexical = 50;
machineGlobalObject = 3;
JSON.stringify([machineGlobalLoads(0), machineGlobalLoads(1)]);
"#,
        "jit-machine-globals-probe.js",
    );
    let (entries, deopts) = stats_delta(before, compiled.execution_stats());
    assert_eq!(actual, expected);
    assert!(entries >= 2, "both probes must enter Machine code");
    assert_eq!(deopts, 0, "live value updates preserve both guards");
}

fn assert_exact_global_object_miss(mutation: &str, module: &str) {
    let mut compiled = compiled_fixture(false);
    let before = compiled.execution_stats();
    let first = completion(
        &mut compiled,
        &format!("{mutation}\nmachineGlobalLoads(0);"),
        module,
    );
    let after_first = compiled.execution_stats();
    let (entries, deopts) = stats_delta(before, after_first);
    assert_eq!(first, "42");
    assert!(entries >= 1, "the stale guard must execute before its miss");
    assert_eq!(
        deopts, 0,
        "a stale global-object guard completes through the cold binding path, not a deopt"
    );

    let reused = completion(
        &mut compiled,
        "machineGlobalLoads(1);",
        "jit-machine-globals-reuse.js",
    );
    let (reuse_entries, reuse_deopts) = stats_delta(after_first, compiled.execution_stats());
    assert_eq!(reused, "43");
    assert!(
        reuse_entries >= 1,
        "the generated body stays installed and reusable after the miss"
    );
    assert_eq!(
        reuse_deopts, 0,
        "the cold path keeps answering without invalidating the generation"
    );
}

#[test]
fn machine_global_object_epoch_miss_takes_the_cold_binding_path() {
    assert_exact_global_object_miss(
        "let machineGlobalEpochBump = 1;",
        "jit-machine-globals-epoch-miss.js",
    );
}

#[test]
fn machine_global_object_shape_miss_takes_the_cold_binding_path() {
    assert_exact_global_object_miss(
        "globalThis.machineGlobalShapeBump = 1;",
        "jit-machine-globals-shape-miss.js",
    );
}
