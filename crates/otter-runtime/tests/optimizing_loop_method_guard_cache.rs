//! Regression coverage for activation-local loop method-guard caches.
//!
//! # Contents
//! - Multiple invariant Map and Math receivers in one natural loop.
//! - Changing primitive-string receivers sharing one pinned prototype method.
//! - Interpreter parity and artifact proof for every cached intrinsic site.
//!
//! # Invariants
//! - Entry and OSR activations start with empty caches.
//! - A changing exotic receiver is validated on every iteration; only its
//!   pinned prototype method identity is reused.
//! - Any generated intrinsic miss clears every raw cached receiver before the
//!   canonical transition may allocate, collect, or re-enter JavaScript.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const LOOP_GUARD_MATRIX: &str = r#"
function guardedLoop(table, math, first, second, needle, limit) {
  let checksum = 0;
  for (let index = 0; index < limit; index++) {
    let word = first;
    if ((index & 1) !== 0) word = second;
    const key = index & 7;
    table.set(key, word);
    checksum += table.get(key).charCodeAt(index & 3);
    checksum += word.indexOf(needle);
    checksum += math.abs((index & 15) - 8);
  }
  return checksum;
}

const table = new Map();
for (let key = 0; key < 8; key++) table.set(key, "otter");
for (let warm = 0; warm < 4010; warm++) {
  guardedLoop(table, Math, "engine", "runtime", "e", 16);
}

function fallbackLoop(table, word, plainNeedle, coerciveNeedle, limit) {
  let checksum = 0;
  for (let index = 0; index < limit; index++) {
    checksum += table.get(0) === word ? 1 : 10;
    let needle = plainNeedle;
    if (index === 31) needle = coerciveNeedle;
    checksum += word.indexOf(needle);
  }
  return checksum;
}

const originalGet = Map.prototype.get;
let coercions = 0;
const coerciveNeedle = {
  toString() {
    coercions += 1;
    Map.prototype.get = function() { return "replacement"; };
    for (let index = 0; index < 300; index++) {
      ({ index, payload: [index, index + 1, index + 2] });
    }
    return "e";
  }
};
const guarded = guardedLoop(table, Math, "engine", "runtime", "e", 1024);
const fallback = fallbackLoop(table, "engine", "e", coerciveNeedle, 64);
Map.prototype.get = originalGet;

function nestedMaps(first, second, limit) {
  let checksum = 0;
  for (let outer = 0; outer < limit; outer++) {
    let current = first;
    if ((outer & 1) !== 0) current = second;
    for (let inner = 0; inner < 8; inner++) {
      checksum += current.get(0);
    }
  }
  return checksum;
}
const firstMap = new Map([[0, 1]]);
const secondMap = new Map([[0, 10]]);
const nested = nestedMaps(firstMap, secondMap, 64);
JSON.stringify([guarded, table.size, fallback, coercions, nested]);
"#;

fn run(selection: JitSelection, artifacts: bool) -> (String, u64, usize) {
    let builder = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(4);
    let mut runtime = if artifacts {
        builder
            .jit_debug(otter_runtime::JitDebugRequest::artifacts())
            .build()
    } else {
        builder.build()
    }
    .expect("loop guard-cache runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(LOOP_GUARD_MATRIX),
            "optimizing-loop-method-guard-cache.js",
        )
        .expect("loop guard-cache matrix");
    let cache_regions = completion.jit_artifacts().map_or(0, |batch| {
        batch
            .bundles()
            .iter()
            .filter_map(|bundle| bundle.file(otter_runtime::JitArtifactFileName::CodeMap))
            .map(|file| {
                let map: serde_json::Value =
                    serde_json::from_slice(file.contents()).expect("valid code-map JSON");
                map["regions"].as_array().map_or(0, |regions| {
                    regions
                        .iter()
                        .filter(|region| region["kind"] == "loopInvariantMethodGuardCache")
                        .count()
                })
            })
            .max()
            .unwrap_or(0)
    });
    (
        completion.completion_string().to_owned(),
        runtime.execution_stats().jit_optimized_entries,
        cache_regions,
    )
}

#[test]
fn loop_method_guard_caches_preserve_semantics() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly, false);
    let (compiled, optimized_entries, _) = run(JitSelection::ProductionTiered, false);
    assert_eq!(compiled, oracle);
    assert_eq!(oracle, "[119040,8,352,1,2816]");
    #[cfg(target_arch = "aarch64")]
    assert!(optimized_entries > 0, "fixture must enter optimizing code");
}

#[cfg(target_arch = "aarch64")]
#[test]
fn artifacts_expose_every_loop_method_guard_cache() {
    let (compiled, optimized_entries, cache_regions) = run(JitSelection::ProductionTiered, true);
    assert_eq!(compiled, "[119040,8,352,1,2816]");
    assert!(optimized_entries > 0, "fixture must enter optimizing code");
    assert_eq!(
        cache_regions, 5,
        "Map.set/get, charCodeAt, indexOf, and Math.abs each need one cache"
    );
}
