//! Machine IR loop-scoped packed-double view-cache coverage.
//!
//! # Contents
//! - A Navier-shaped kernel with two inner loops, four invariant Array
//!   receivers, and ten packed-double element sites.
//! - Whole-function entry and loop-OSR compilation through the production tier
//!   policy.
//! - Fresh replacement arrays after a full collection and a later generated
//!   entry into the same code object.
//!
//! # Invariants
//! - Ten direct packed-double sites share exactly four loop-scoped raw views:
//!   one per receiver and natural loop.
//! - Function entry, OSR entry, and every external inner-loop entry start from
//!   cleared raw base words; a cache survives only a generated backedge.
//! - Cached words are untraced base/length data, never GC roots. A later call
//!   after moving collection must prove and publish each fresh Array view.
//! - Supported packed-double inputs execute without a generic element stub or
//!   an optimizing deoptimization.
//!
//! # See also
//! - `otter_jit::machine::numeric` owns cache planning, Machine selection, raw
//!   frame layout, and AArch64 emission.

#![cfg(target_arch = "aarch64")]

use std::collections::{BTreeMap, BTreeSet};

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTarget,
    JitDebugTier, JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";
const ENTRY_MODULE: &str = "jit-machine-packed-cache-entry-setup.js";
const OSR_MODULE: &str = "jit-machine-packed-cache-osr-setup.js";
const KERNEL: &str = r#"
function machinePackedViewCacheKernel(inputA, outputA, inputB, outputB, rounds) {
  let checksum = 0;
  for (let round = 0; round < rounds; round = round + 1) {
    for (let index = 1; index < 5; index = index + 1) {
      const value = (
        inputA[index - 1] +
        inputA[index] +
        inputA[index + 1] +
        inputA[index + 2]
      ) * 0.25;
      outputA[index] = value;
      checksum = checksum + value;
    }
    for (let index = 1; index < 5; index = index + 1) {
      const value = (
        inputB[index - 1] +
        inputB[index] +
        inputB[index + 1] +
        inputB[index + 2]
      ) * 0.25;
      outputB[index] = value;
      checksum = checksum + value;
    }
  }
  return checksum;
}

function makePackedViewCacheInputs() {
  return [
    [0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5],
    [0.25, 0.25, 0.25, 0.25, 0.25, 0.25, 0.25],
    [10.5, 20.5, 30.5, 40.5, 50.5, 60.5, 70.5],
    [0.25, 0.25, 0.25, 0.25, 0.25, 0.25, 0.25]
  ];
}
"#;

const ENTRY_SETUP: &str = r#"
globalThis.__packedCacheWarm = makePackedViewCacheInputs();
for (let warm = 0; warm < 5000; warm = warm + 1) {
  machinePackedViewCacheKernel(
    __packedCacheWarm[0],
    __packedCacheWarm[1],
    __packedCacheWarm[2],
    __packedCacheWarm[3],
    1
  );
}
"#;

const OSR_SETUP: &str = r#"
globalThis.__packedCacheOsr = makePackedViewCacheInputs();
// Each inner header sees 32 prepared backedges without reaching threshold 36.
// The next call therefore enters optimizing OSR with every element site hot.
for (let warm = 0; warm < 8; warm = warm + 1) {
  machinePackedViewCacheKernel(
    __packedCacheOsr[0],
    __packedCacheOsr[1],
    __packedCacheOsr[2],
    __packedCacheOsr[3],
    1
  );
}
globalThis.__packedCacheOsrChecksum = machinePackedViewCacheKernel(
  __packedCacheOsr[0],
  __packedCacheOsr[1],
  __packedCacheOsr[2],
  __packedCacheOsr[3],
  40
);
JSON.stringify([
  __packedCacheOsrChecksum,
  __packedCacheOsr[1],
  __packedCacheOsr[3]
]);
"#;

const FRESH_PROBE: &str = r#"
globalThis.__packedCacheFreshInputA = [1.25, 2.25, 3.25, 4.25, 5.25, 6.25, 7.25];
globalThis.__packedCacheFreshOutputA = [0.75, 0.75, 0.75, 0.75, 0.75, 0.75, 0.75];
globalThis.__packedCacheFreshInputB = [
  100.5,
  200.5,
  300.5,
  400.5,
  500.5,
  600.5,
  700.5
];
globalThis.__packedCacheFreshOutputB = [0.75, 0.75, 0.75, 0.75, 0.75, 0.75, 0.75];
globalThis.__packedCacheFreshChecksum = machinePackedViewCacheKernel(
  __packedCacheFreshInputA,
  __packedCacheFreshOutputA,
  __packedCacheFreshInputB,
  __packedCacheFreshOutputB,
  2
);
JSON.stringify([
  __packedCacheFreshChecksum,
  __packedCacheFreshOutputA,
  __packedCacheFreshOutputB
]);
"#;

const ENTRY_EXPECTED: &str = concat!(
    "[3238,",
    "[0.75,2.75,3.75,4.75,5.75,0.75,0.75],",
    "[0.75,250.5,350.5,450.5,550.5,0.75,0.75]]"
);
const OSR_EXPECTED: &str = concat!(
    "[7040,",
    "[0.25,2,3,4,5,0.25,0.25],",
    "[0.25,25.5,35.5,45.5,55.5,0.25,0.25]]"
);

#[derive(Debug, Clone, Copy)]
struct CounterDelta {
    optimized_entries: u64,
    optimized_deopts: u64,
}

impl CounterDelta {
    fn between(before: RuntimeExecutionStats, after: RuntimeExecutionStats) -> Self {
        Self {
            optimized_entries: after.jit_optimized_entries - before.jit_optimized_entries,
            optimized_deopts: after.jit_optimized_deopts - before.jit_optimized_deopts,
        }
    }
}

fn runtime(osr_threshold: u32) -> Runtime {
    Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_osr_threshold(osr_threshold)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("packed-double cache runtime")
}

fn completion(runtime: &mut Runtime, source: &str, module: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), module)
        .unwrap_or_else(|error| panic!("packed-double cache fixture {module}: {error:?}"))
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

fn target_matches(actual: JitDebugTarget, expect_osr: bool) -> bool {
    if expect_osr {
        matches!(actual, JitDebugTarget::Osr { .. })
    } else {
        actual == JitDebugTarget::Entry
    }
}

fn machine_cache_bundle<'artifacts>(
    artifacts: &'artifacts JitArtifactBatch,
    module: &str,
    expect_osr: bool,
) -> &'artifacts JitArtifactBundle {
    artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == module
                && manifest.function_name() == "machinePackedViewCacheKernel"
                && manifest.tier() == JitDebugTier::Optimizing
                && target_matches(manifest.entry(), expect_osr)
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| {
                        file.contents().starts_with(MACHINE_IR_HEADER)
                            && file
                                .contents()
                                .windows(b"machine-ir packed-double-view-caches=4".len())
                                .any(|window| window == b"machine-ir packed-double-view-caches=4")
                    })
        })
        .unwrap_or_else(|| {
            let manifests = artifacts
                .bundles()
                .iter()
                .map(|bundle| {
                    let manifest = bundle.manifest();
                    let first_line = bundle
                        .file(JitArtifactFileName::OptimizedIr)
                        .and_then(|file| std::str::from_utf8(file.contents()).ok())
                        .and_then(|ir| ir.lines().next())
                        .unwrap_or("<no optimized IR>");
                    format!(
                        "{}:{}:{:?}:{:?}:{first_line}",
                        manifest.module(),
                        manifest.function_name(),
                        manifest.tier(),
                        manifest.entry()
                    )
                })
                .collect::<Vec<_>>();
            panic!("missing exact packed-cache Machine bundle: {manifests:?}")
        })
}

fn cache_id(line: &str) -> usize {
    line.split_once("cache: Some(PackedDoubleViewCacheId(")
        .and_then(|(_, suffix)| suffix.split_once(')'))
        .and_then(|(id, _)| id.parse().ok())
        .unwrap_or_else(|| panic!("packed-double site lacks a valid cache identity: {line}"))
}

fn assert_machine_cache_artifact(artifacts: &JitArtifactBatch, module: &str, expect_osr: bool) {
    let bundle = machine_cache_bundle(artifacts, module, expect_osr);
    let optimized_ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("packed-cache optimized IR")
            .contents(),
    )
    .expect("UTF-8 packed-cache optimized IR");
    assert_eq!(
        optimized_ir
            .lines()
            .filter(|line| *line == "machine-ir packed-double-view-caches=4")
            .count(),
        1,
        "the Machine frame must own exactly four two-word views: {optimized_ir}"
    );

    let load_lines = optimized_ir
        .lines()
        .filter(|line| line.contains("PackedDoubleElementLoad {"))
        .collect::<Vec<_>>();
    let store_lines = optimized_ir
        .lines()
        .filter(|line| line.contains("PackedDoubleElementStore {"))
        .collect::<Vec<_>>();
    assert_eq!(load_lines.len(), 8, "eight direct FP loads: {optimized_ir}");
    assert_eq!(store_lines.len(), 2, "two direct FP stores: {optimized_ir}");

    let packed_lines = load_lines
        .iter()
        .chain(&store_lines)
        .copied()
        .collect::<Vec<_>>();
    assert!(
        packed_lines
            .iter()
            .all(|line| line.contains("cache: Some(PackedDoubleViewCacheId(")),
        "every packed site must use a persistent loop view: {optimized_ir}"
    );
    let mut sites_per_cache = BTreeMap::<usize, usize>::new();
    for line in &packed_lines {
        *sites_per_cache.entry(cache_id(line)).or_default() += 1;
    }
    assert_eq!(
        sites_per_cache.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([0, 1, 2, 3]),
        "the two natural loops must own four dense cache identities: {optimized_ir}"
    );
    let mut group_sizes = sites_per_cache.values().copied().collect::<Vec<_>>();
    group_sizes.sort_unstable();
    assert_eq!(
        group_sizes,
        [1, 1, 4, 4],
        "each input shares four loads and each output owns one store: {optimized_ir}"
    );

    let clear_count = optimized_ir
        .lines()
        .filter(|line| line.contains("ClearPackedDoubleViewCaches(LoopEntry)"))
        .count();
    assert_eq!(
        clear_count, 2,
        "each external inner-loop entry must clear persistent raw bases: {optimized_ir}"
    );

    let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let regions = code_map["regions"]
        .as_array()
        .expect("packed-cache code-map regions");
    for (kind, expected_count) in [
        ("machinePackedDoubleElementLoad", 8),
        ("machinePackedDoubleElementStore", 2),
        ("machinePackedDoubleViewCacheClear", 2),
    ] {
        let matching = regions
            .iter()
            .filter(|region| region["kind"] == kind)
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            expected_count,
            "unexpected {kind} region count: {code_map}"
        );
        assert!(
            matching
                .iter()
                .all(|region| { region["startOffset"].as_u64() < region["endOffset"].as_u64() }),
            "every {kind} region must own emitted native code: {code_map}"
        );
    }

    let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
    let relocations = relocations["relocations"]
        .as_array()
        .expect("packed-cache relocations");
    assert!(
        relocations.iter().all(|relocation| {
            relocation["target"]["kind"] != "runtimeStub"
                || !matches!(
                    relocation["target"]["name"].as_str(),
                    Some("jit_load_element" | "jit_store_element")
                )
        }),
        "cached packed sites must not retain generic element stubs: {relocations:?}"
    );
    assert_eq!(
        relocations
            .iter()
            .filter(|relocation| relocation["target"]["kind"] == "gcCageBase")
            .count(),
        packed_lines.len(),
        "each exact-deopt site currently owns one skipped lazy-miss proof; the hot path shares four published views"
    );
}

fn run_fresh_after_gc(runtime: &mut Runtime, module: &str) {
    runtime.force_gc().expect("packed-cache full GC");
    let before = runtime.execution_stats();
    let actual = completion(runtime, FRESH_PROBE, module);
    let delta = CounterDelta::between(before, runtime.execution_stats());
    assert_eq!(actual, ENTRY_EXPECTED);
    assert!(
        delta.optimized_entries > 0,
        "fresh arrays must enter the existing Machine code: {delta:?}"
    );
    assert_eq!(
        delta.optimized_deopts, 0,
        "entry/loop clears must prevent stale raw views after GC: {delta:?}"
    );
}

#[test]
fn packed_double_views_share_by_loop_receiver_and_refresh_after_gc() {
    let mut runtime = runtime(u32::MAX);
    let setup_source = format!("{KERNEL}\n{ENTRY_SETUP}");
    let setup = runtime
        .run_script(SourceInput::from_javascript(setup_source), ENTRY_MODULE)
        .expect("packed-cache entry setup");
    assert_machine_cache_artifact(
        setup.jit_artifacts().expect("packed-cache entry artifacts"),
        ENTRY_MODULE,
        false,
    );
    drop(setup);

    run_fresh_after_gc(&mut runtime, "jit-machine-packed-cache-entry-fresh.js");
}

#[test]
fn packed_double_views_start_cleared_at_osr_and_later_entry() {
    let mut runtime = runtime(36);
    let setup_source = format!("{KERNEL}\n{OSR_SETUP}");
    let before = runtime.execution_stats();
    let setup = runtime
        .run_script(SourceInput::from_javascript(setup_source), OSR_MODULE)
        .expect("packed-cache OSR setup");
    let delta = CounterDelta::between(before, runtime.execution_stats());
    assert_eq!(setup.completion_string(), OSR_EXPECTED);
    assert!(
        runtime.execution_stats().jit_osr_attempts > before.jit_osr_attempts,
        "fixture must enter through a loop OSR header"
    );
    assert!(
        delta.optimized_entries > 0,
        "the threshold-crossing call must execute optimizing OSR: {delta:?}"
    );
    assert_eq!(
        delta.optimized_deopts, 0,
        "OSR entry and later external loop entries must begin cleared: {delta:?}"
    );
    assert_machine_cache_artifact(
        setup.jit_artifacts().expect("packed-cache OSR artifacts"),
        OSR_MODULE,
        true,
    );
    drop(setup);

    run_fresh_after_gc(&mut runtime, "jit-machine-packed-cache-osr-fresh.js");
}
