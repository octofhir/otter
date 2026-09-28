//! Machine IR direct global-load coverage.
//!
//! # Contents
//! - Live global-declarative cell and guarded global-object reads.
//! - A global-declarative epoch miss that deoptimizes exactly once, and a
//!   dictionary global-object add that keeps the guard valid.
//! - Machine code-map, relocation, deopt, and empty-safepoint proofs.
//!
//! # Invariants
//! - A prepared global load reads the live tagged slot without a runtime call.
//! - Lexical TDZ, global-declarative epoch, and global-object shape misses branch
//!   to the source operation's exact deopt before publishing an effect.
//! - Artifacts name the lexical cell symbolically and never serialize its
//!   process address.
//! - The setup warms `machineGlobalLoads` from short scripts ([`short_warm`]),
//!   so the function compiles through its own entries instead of being
//!   inlined into an OSR-compiled script body.

use std::collections::BTreeSet;

use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    RuntimeExecutionStats, SourceInput,
};

#[path = "support/short_warm.rs"]
mod short_warm;

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";

const SETUP: &str = r#"
let machineGlobalLexical = 40;
globalThis.machineGlobalObject = 2;

function machineGlobalLoads(bias) {
  let result = machineGlobalLexical + machineGlobalObject + bias;
  for (let index = 0; index < 2; index++) result += 0;
  return result;
}
"#;

const WARM_CALL: &str = "machineGlobalLoads(i & 7);";

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

fn machine_global_bundle(warm: &short_warm::WarmRuns) -> &JitArtifactBundle {
    warm.bundles()
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
                                ["machineBindingGuard", "machineBindingHit"]
                                    .into_iter()
                                    .all(|kind| regions.iter().any(|region| region["kind"] == kind))
                            },
                        )
                    })
        })
        .unwrap_or_else(|| {
            let manifests = warm
                .bundles()
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
    // Each global read is one speculative Machine binding: a guard on the
    // live cell (lexical) or on the global object's shape and declarative
    // epoch (object), whose miss deoptimizes at the read before any effect,
    // and a hit that reads the slot. No read owns a runtime call.
    let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let regions = code_map["regions"].as_array().expect("code-map regions");
    let mut global_byte_pcs = BTreeSet::new();
    for kind in ["machineBindingGuard", "machineBindingHit"] {
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
                    || relocation["target"]["name"] == "jit_backedge_poll")
        }),
        "global reads link no runtime transition; only the loop poll, exact-deopt \
         handler and abrupt-completion finisher remain: {relocations:?}"
    );

    // Every global-read site owns an exact exit whose frame state resumes at
    // that read.
    let deopt = artifact_json(bundle, JitArtifactFileName::Deopt);
    let frame_state_byte_pcs = deopt["frameStates"]
        .as_array()
        .expect("deopt frame states")
        .iter()
        .filter_map(|state| {
            let id = state["id"].as_u64()?;
            let byte_pc = state["frames"].as_array()?.last()?["bytePc"].as_u64()?;
            Some((id, byte_pc))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let exit_byte_pcs = deopt["exits"]
        .as_array()
        .expect("deopt exits")
        .iter()
        .filter_map(|exit| {
            frame_state_byte_pcs
                .get(&exit["frameStateId"].as_u64()?)
                .copied()
        })
        .collect::<BTreeSet<_>>();
    assert!(
        global_byte_pcs.is_subset(&exit_byte_pcs),
        "global guards miss to an exact deopt at their read: {deopt}"
    );

    let safepoints = artifact_json(bundle, JitArtifactFileName::Safepoints);
    assert_eq!(
        safepoints["safepoints"].as_array().map(Vec::len),
        Some(0),
        "no global read reenters the runtime or safepoints"
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
    runtime
        .run_script(
            SourceInput::from_javascript(SETUP),
            "jit-machine-globals-setup.js",
        )
        .expect("Machine global-load setup");
    let warm = short_warm::warm(&mut runtime, WARM_CALL, 3000, "jit-machine-globals-warm.js");
    if artifacts {
        assert_machine_global_artifact(machine_global_bundle(&warm));
    }
    drop(warm);
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

fn assert_global_object_miss(mutation: &str, module: &str, expected_deopts: u64) {
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
        deopts, expected_deopts,
        "a stale global-object guard deoptimizes exactly at its read"
    );

    let reused = completion(
        &mut compiled,
        "machineGlobalLoads(1);",
        "jit-machine-globals-reuse.js",
    );
    let (reuse_entries, reuse_deopts) = stats_delta(after_first, compiled.execution_stats());
    assert_eq!(reused, "43");
    if expected_deopts == 0 {
        assert!(
            reuse_entries >= 1,
            "the generated body stays installed and reusable"
        );
    }
    assert_eq!(
        reuse_deopts, 0,
        "a miss never repeats: the next call runs a valid generation or the interpreter"
    );
}

#[test]
fn machine_global_object_epoch_miss_deoptimizes_exactly_once() {
    assert_global_object_miss(
        "let machineGlobalEpochBump = 1;",
        "jit-machine-globals-epoch-miss.js",
        1,
    );
}

#[test]
fn machine_global_object_dictionary_add_keeps_the_guard() {
    assert_global_object_miss(
        "globalThis.machineGlobalShapeBump = 1;",
        "jit-machine-globals-shape-miss.js",
        0,
    );
}
