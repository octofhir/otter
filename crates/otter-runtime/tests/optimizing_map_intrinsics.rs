//! Regression coverage for optimizing compact-Map intrinsics.
//!
//! # Contents
//! - Direct `Map.get(Int32)` and existing-key `Map.set(Int32, value)`.
//! - Missing keys, tombstones, growth, SameValueZero fallback, and barriers.
//! - Method replacement and own-method shadowing after tier-up.
//! - Artifact proof that generated hits retain neither a Rust leaf ABI nor a
//!   published transition-frame prelude.
//!
//! # Invariants
//! - Optimizing results match the interpreter oracle exactly.
//! - A failed `set` probe has no effect before the canonical insertion path.
//! - Pointer overwrites remain visible after stress collection.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const MAP_MATRIX: &str = r#"
function getInt(table, key) {
  return table.get(key | 0);
}

function setInt(table, key, value) {
  return table.set(key | 0, value);
}

const table = new Map();
for (let index = 0; index < 64; index++) {
  table.set(index, "v" + index);
}
for (let warm = 0; warm < 4010; warm++) {
  const key = warm & 63;
  setInt(table, key, "v" + key);
  getInt(table, key);
}

const result = [];
result.push(getInt(table, 17));
result.push(getInt(table, 999) === undefined);
result.push(setInt(table, 17, "changed") === table);
result.push(getInt(table, 17));

const marker = { value: 41 };
setInt(table, 18, marker);
for (let index = 0; index < 300; index++) {
  ({ index, payload: [index, index + 1, index + 2] });
}
result.push(getInt(table, 18) === marker);
result.push(getInt(table, 18).value);

table.delete(19);
result.push(getInt(table, 19) === undefined);
result.push(setInt(table, 19, "restored") === table);
result.push(getInt(table, 19));

const zero = new Map();
zero.set(-0, "zero");
result.push(getInt(zero, 0));
setInt(zero, 0, "updated-zero");
result.push(zero.size);
result.push(getInt(zero, 0));

result.push(setInt(table, 1000, "inserted") === table);
result.push(getInt(table, 1000));
for (let index = 64; index < 200; index++) {
  table.set(index, "g" + index);
}
result.push(getInt(table, 199));

const originalGet = Map.prototype.get;
Map.prototype.get = function() { return 777; };
result.push(getInt(table, 1));
Map.prototype.get = originalGet;
result.push(getInt(table, 1));

table.get = function() { return 888; };
result.push(getInt(table, 2));
delete table.get;
result.push(getInt(table, 2));

JSON.stringify(result);
"#;

fn run(selection: JitSelection) -> (String, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(4)
        .build()
        .expect("Map intrinsic runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(MAP_MATRIX),
            "optimizing-map-intrinsics.js",
        )
        .expect("Map intrinsic matrix")
        .completion_string()
        .to_owned();
    (completion, runtime.execution_stats().jit_optimized_entries)
}

#[test]
fn optimizing_map_intrinsics_match_oracle_and_preserve_fallbacks() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, optimized_entries) = run(JitSelection::ProductionTiered);

    assert_eq!(compiled, oracle);
    assert_eq!(
        oracle,
        r#"["v17",true,true,"changed",true,41,true,true,"restored","zero",1,"updated-zero",true,"inserted","g199",777,"v1",888,"v2"]"#
    );
    #[cfg(target_arch = "aarch64")]
    assert!(
        optimized_entries > 0,
        "fixture must enter optimized Map callers before fallback cases"
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn optimizing_map_artifacts_expose_frame_free_machine_hits() {
    use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitDebugTier};

    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_osr_threshold(4)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("Map artifact runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(MAP_MATRIX),
            "optimizing-map-artifacts.js",
        )
        .expect("Map artifact matrix");
    let artifacts = result.jit_artifacts().expect("enabled artifact batch");
    let optimizing: Vec<_> = artifacts
        .bundles()
        .iter()
        .filter(|bundle| bundle.manifest().tier() == JitDebugTier::Optimizing)
        .collect();

    assert!(optimizing.len() >= 2, "both Map callers must optimize");
    let relocations = optimizing
        .iter()
        .filter_map(|bundle| bundle.file(JitArtifactFileName::Relocations))
        .map(|file| std::str::from_utf8(file.contents()).expect("relocations are UTF-8"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !relocations.contains("collection_map_get_leaf"),
        "Map.get must complete without the Rust leaf ABI: {relocations}"
    );
    assert!(
        !relocations.contains("collection_map_set_mutating"),
        "existing-key Map.set must complete without the Rust leaf ABI: {relocations}"
    );
    let machine_intrinsics = optimizing
        .iter()
        .filter_map(|bundle| bundle.file(JitArtifactFileName::CodeMap))
        .map(|file| {
            let map: serde_json::Value =
                serde_json::from_slice(file.contents()).expect("valid code-map JSON");
            map["regions"].as_array().map_or(0, |regions| {
                regions
                    .iter()
                    .filter(|region| region["kind"] == "machineMethodIntrinsic")
                    .count()
            })
        })
        .sum::<usize>();
    assert!(
        machine_intrinsics >= 2,
        "Map.get and Map.set must each expose a frame-free machine hit"
    );
}
