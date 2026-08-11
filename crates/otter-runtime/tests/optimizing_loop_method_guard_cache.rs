//! Regression coverage for activation-local loop method-guard caches.
//!
//! # Contents
//! - Multiple invariant Map and Math receivers in one natural loop.
//! - Changing primitive-string receivers sharing one pinned prototype method.
//! - Global-lexical, dense-element, and exotic-length reads in a cached loop.
//! - Global-object slot caching that makes a builtin namespace receiver
//!   activation-invariant after one epoch/shape proof.
//! - Element/property slow reads that mutate a cached method during reentry.
//! - Interpreter parity and artifact proof for every cached intrinsic site.
//!
//! # Invariants
//! - Entry and OSR activations start with empty caches.
//! - A changing exotic receiver is validated on every iteration; only its
//!   pinned prototype method identity is reused.
//! - Any generated intrinsic miss clears every raw cached receiver before the
//!   canonical transition may allocate, collect, or re-enter JavaScript.
//! - Any element/property/global probe miss applies the same invalidation
//!   before its canonical lookup transition.
//! - A cached global retains only a raw live-slot address; reentry clears it
//!   together with method headers before moving GC or observable mutation.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const LOOP_GUARD_MATRIX: &str = r#"
function guardedLoop(table, math, needle, limit) {
  let checksum = 0;
  for (let index = 0; index < limit; index++) {
    const word = words[index & 1];
    const key = index & 7;
    table.set(key, word);
    checksum += table.get(key).charCodeAt(index & 3);
    checksum += word.indexOf(needle);
    checksum += word.length;
    checksum += math.abs((index & 15) - 8);
  }
  return checksum;
}

const table = new Map();
for (let key = 0; key < 8; key++) table.set(key, "otter");
const words = ["engine", "runtime"];
for (let warm = 0; warm < 4010; warm++) {
  guardedLoop(table, Math, "e", 16);
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
const guarded = guardedLoop(table, Math, "e", 1024);
const fallback = fallbackLoop(table, "engine", "e", coerciveNeedle, 64);
Map.prototype.get = originalGet;

function elementMissLoop(table, values, limit) {
  let checksum = 0;
  for (let index = 0; index < limit; index++) {
    checksum += table.get(0) === 1 ? 1 : 10;
    checksum += values[index];
  }
  return checksum;
}

const elementValues = [];
for (let index = 0; index < 64; index++) elementValues[index] = 1;
Object.defineProperty(elementValues, 31, {
  configurable: true,
  get() {
    Map.prototype.get = function() { return 10; };
    for (let index = 0; index < 300; index++) ({ payload: [index, index + 1] });
    return 5;
  }
});
const elementTable = new Map([[0, 1]]);
for (let warm = 0; warm < 4010; warm++) elementMissLoop(elementTable, [1, 1], 2);
const elementMiss = elementMissLoop(elementTable, elementValues, 64);
Map.prototype.get = originalGet;

function propertyMissLoop(table, values, limit) {
  let checksum = 0;
  for (let index = 0; index < limit; index++) {
    checksum += table.get(0) === 1 ? 1 : 10;
    checksum += values[index].flag;
  }
  return checksum;
}

const propertyValues = [];
for (let index = 0; index < 64; index++) propertyValues[index] = { flag: 1 };
Object.defineProperty(propertyValues[31], "flag", {
  configurable: true,
  get() {
    Map.prototype.get = function() { return 10; };
    for (let index = 0; index < 300; index++) ({ payload: [index, index + 1] });
    return 5;
  }
});
const propertyTable = new Map([[0, 1]]);
for (let warm = 0; warm < 4010; warm++) propertyMissLoop(propertyTable, [{ flag: 1 }, { flag: 1 }], 2);
const propertyMiss = propertyMissLoop(propertyTable, propertyValues, 64);
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

function globalMathLoop(values, limit) {
  let checksum = 0;
  for (let index = 0; index < limit; index++) {
    checksum += Math.abs(-1);
    checksum += Math.max(index & 3, 2);
    checksum += values[index].flag;
  }
  return checksum;
}

for (let warm = 0; warm < 4010; warm++) {
  globalMathLoop([{ flag: 1 }, { flag: 1 }], 2);
}
const globalFastValues = [];
for (let index = 0; index < 64; index++) globalFastValues[index] = { flag: 1 };
const globalFast = globalMathLoop(globalFastValues, 64);

const originalMath = Math;
let replacementCalls = 0;
const replacementMath = {
  abs() {
    replacementCalls += 1;
    return 9;
  },
  max() {
    replacementCalls += 1;
    return 7;
  }
};
const globalReentryValues = [];
for (let index = 0; index < 64; index++) globalReentryValues[index] = { flag: 1 };
Object.defineProperty(globalReentryValues[31], "flag", {
  configurable: true,
  get() {
    Math = replacementMath;
    for (let index = 0; index < 300; index++) ({ payload: [index, index + 1] });
    return 5;
  }
});
const globalReentry = globalMathLoop(globalReentryValues, 64);
Math = originalMath;

JSON.stringify([
  guarded,
  table.size,
  fallback,
  coercions,
  elementMiss,
  propertyMiss,
  nested,
  globalFast,
  globalReentry,
  replacementCalls
]);
"#;

fn run(
    selection: JitSelection,
    artifacts: bool,
) -> (String, u64, usize, usize, usize, usize, usize) {
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
    let cache_region_counts = completion.jit_artifacts().map_or_else(Vec::new, |batch| {
        batch
            .bundles()
            .iter()
            .filter_map(|bundle| bundle.file(otter_runtime::JitArtifactFileName::CodeMap))
            .map(|file| {
                let map: serde_json::Value =
                    serde_json::from_slice(file.contents()).expect("valid code-map JSON");
                map["regions"].as_array().map_or((0, 0), |regions| {
                    let methods = regions
                        .iter()
                        .filter(|region| region["kind"] == "loopInvariantMethodGuardCache")
                        .count();
                    let globals = regions
                        .iter()
                        .filter(|region| region["kind"] == "loopInvariantGlobalObjectLoadCache")
                        .count();
                    (methods, globals)
                })
            })
            .collect()
    });
    (
        completion.completion_string().to_owned(),
        runtime.execution_stats().jit_optimized_entries,
        cache_region_counts
            .iter()
            .map(|counts| counts.0)
            .max()
            .unwrap_or(0),
        cache_region_counts
            .iter()
            .filter(|counts| counts.0 > 0)
            .count(),
        cache_region_counts
            .iter()
            .map(|counts| counts.1)
            .max()
            .unwrap_or(0),
        cache_region_counts
            .iter()
            .filter(|counts| counts.1 > 0)
            .count(),
        cache_region_counts
            .iter()
            .filter(|counts| counts.1 > 0)
            .map(|counts| counts.0)
            .max()
            .unwrap_or(0),
    )
}

#[test]
fn loop_method_guard_caches_preserve_semantics() {
    let (oracle, _, _, _, _, _, _) = run(JitSelection::InterpreterOnly, false);
    let (compiled, optimized_entries, _, _, _, _, _) = run(JitSelection::ProductionTiered, false);
    assert_eq!(compiled, oracle);
    assert_eq!(oracle, "[125696,8,352,1,420,420,2816,272,684,64]");
    #[cfg(target_arch = "aarch64")]
    assert!(optimized_entries > 0, "fixture must enter optimizing code");
}

#[cfg(target_arch = "aarch64")]
#[test]
fn artifacts_expose_every_loop_method_guard_cache() {
    let (
        compiled,
        optimized_entries,
        cache_regions,
        cache_bundles,
        global_cache_regions,
        global_cache_bundles,
        global_receiver_method_regions,
    ) = run(JitSelection::ProductionTiered, true);
    assert_eq!(compiled, "[125696,8,352,1,420,420,2816,272,684,64]");
    assert!(optimized_entries > 0, "fixture must enter optimizing code");
    assert_eq!(
        cache_regions, 5,
        "Map.set/get, charCodeAt, indexOf, and Math.abs each need one cache"
    );
    assert!(
        cache_bundles >= 3,
        "the mixed fast loop and both reentrant read loops must each cache a method guard"
    );
    assert_eq!(
        global_cache_regions, 2,
        "both global Math reads need activation-local live-slot caches"
    );
    assert!(
        global_cache_bundles >= 1,
        "the global Math loop must publish its live-slot cache"
    );
    assert!(
        global_receiver_method_regions >= 2,
        "cached Math values must unlock both namespace method guards"
    );
}
