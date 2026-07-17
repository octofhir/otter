//! Moving-GC invariants for native collection callbacks and constructors.
//!
//! # Contents
//! - Sloppy primitive `thisArg` boxing for Array, Map, and Set callbacks.
//! - Map/Set receiver, key, value, and captured-state retention across callback
//!   allocations.
//! - `Map.getOrInsertComputed` re-scan state and returned-value retention.
//! - TypedArray reduce/find/filter callback state, including BigInt elements.
//! - Array sort/toSorted comparator operands retained across allocation.
//! - `Reflect.construct` of the native Map constructor with a custom
//!   `newTarget` prototype.
//!
//! # Invariants
//! - Callback arguments, receivers, captures, accumulators, and comparator
//!   operands remain rooted while native collection loops re-enter JavaScript.
//! - A sloppy callback boxes its primitive receiver separately for every call;
//!   a relocated wrapper is neither lost nor reused for the next invocation.
//! - Native constructor `this`/`newTarget` state remains live through prototype
//!   lookup and result publication.
//! - A host-forced full collection between warmup and observation preserves all
//!   fixture state. `OTTER_GC_STRESS=full` additionally relocates values at the
//!   allocation boundaries inside each callback.

use otter_runtime::{JitSelection, Runtime, SourceInput};

struct RunResult {
    completion: String,
    compile_attempts: u64,
}

fn run_after_full_gc(selection: JitSelection, setup: &str, probe: &str, name: &str) -> RunResult {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("runtime");
    let setup_name = format!("{name}-setup.js");
    runtime
        .run_script(SourceInput::from_javascript(setup.to_string()), &setup_name)
        .expect("fixture warmup");

    // This is the public force-GC boundary available to embedders. The stress
    // run also forces collections inside the allocating JavaScript callbacks.
    runtime.force_gc().expect("full GC between turns");

    let probe_name = format!("{name}-probe.js");
    let completion = runtime
        .run_script(SourceInput::from_javascript(probe.to_string()), &probe_name)
        .expect("fixture probe")
        .completion_string()
        .to_owned();
    RunResult {
        completion,
        compile_attempts: runtime.execution_stats().jit_compile_attempts,
    }
}

fn assert_interpreter_and_template(setup: &str, probe: &str, name: &str, expected: &str) {
    let oracle = run_after_full_gc(JitSelection::InterpreterOnly, setup, probe, name);
    let compiled = run_after_full_gc(JitSelection::Template, setup, probe, name);
    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, expected);
    assert!(
        compiled.compile_attempts > 0,
        "{name} must exercise template compilation"
    );
}

const SLOPPY_THIS_SETUP: &str = r#"
globalThis.__gcSloppyThisFixture = (() => {
  const array = [11, 12, 13];
  const map = new Map([[{ id: 1 }, 21], [{ id: 2 }, 22], [{ id: 3 }, 23]]);
  const set = new Set([{ id: 31 }, { id: 32 }, { id: 33 }]);

  function churn(seed) {
    let tail = null;
    for (let i = 0; i < 6; i++) {
      tail = { seed, i, text: "box-" + seed + "-" + i, tail };
    }
    return tail.i;
  }

  function observeArray() {
    let previous = null;
    let count = 0;
    let fresh = true;
    let boxed = true;
    let ownerOk = true;
    array.forEach(function (value, index, owner) {
      const held = this;
      boxed = boxed && typeof held === "object" && held.valueOf() === 7;
      fresh = fresh && (previous === null || previous !== held);
      ownerOk = ownerOk && owner === array && value === 11 + index;
      previous = held;
      churn(value + index);
      boxed = boxed && held.valueOf() === 7;
      count++;
    }, 7);
    return [count, fresh, boxed, ownerOk];
  }

  function observeMap() {
    let previous = null;
    let count = 0;
    let fresh = true;
    let boxed = true;
    let ownerOk = true;
    map.forEach(function (value, key, owner) {
      const held = this;
      boxed = boxed && typeof held === "object" && held.valueOf() === 7;
      fresh = fresh && (previous === null || previous !== held);
      ownerOk = ownerOk && owner === map && value === 20 + key.id;
      previous = held;
      churn(value + key.id);
      boxed = boxed && held.valueOf() === 7;
      count++;
    }, 7);
    return [count, fresh, boxed, ownerOk];
  }

  function observeSet() {
    let previous = null;
    let count = 0;
    let fresh = true;
    let boxed = true;
    let ownerOk = true;
    set.forEach(function (value, key, owner) {
      const held = this;
      boxed = boxed && typeof held === "object" && held.valueOf() === 7;
      fresh = fresh && (previous === null || previous !== held);
      ownerOk = ownerOk && owner === set && value === key;
      previous = held;
      churn(value.id);
      boxed = boxed && held.valueOf() === 7;
      count++;
    }, 7);
    return [count, fresh, boxed, ownerOk];
  }

  for (let i = 0; i < 96; i++) {
    observeArray();
    observeMap();
    observeSet();
  }
  return { probe: () => [observeArray(), observeMap(), observeSet()] };
})();
"#;

#[test]
fn sloppy_collection_callbacks_keep_fresh_rooted_primitive_wrappers() {
    assert_interpreter_and_template(
        SLOPPY_THIS_SETUP,
        "JSON.stringify(__gcSloppyThisFixture.probe());",
        "gc-sloppy-collection-this",
        "[[3,true,true,true],[3,true,true,true],[3,true,true,true]]",
    );
}

const MAP_SET_DATA_SETUP: &str = r#"
globalThis.__gcMapSetFixture = (() => {
  const marker = { name: "retained-marker", version: 9 };
  const keys = [{ id: 1 }, { id: 2 }, { id: 3 }];
  const values = [{ score: 10 }, { score: 20 }, { score: 30 }];
  const map = new Map([[keys[0], values[0]], [keys[1], values[1]], [keys[2], values[2]]]);
  const setValues = [{ id: 4 }, { id: 5 }, { id: 6 }];
  const set = new Set(setValues);

  function churn(seed) {
    let result = null;
    for (let i = 0; i < 8; i++) result = { seed, i, result };
    return result;
  }

  function visitMap() {
    let total = 0;
    let identity = true;
    map.forEach(function (value, key, owner) {
      const heldKey = key;
      const heldValue = value;
      const heldOwner = owner;
      const heldMarker = marker;
      const allocation = churn(key.id);
      identity = identity && heldOwner === map;
      identity = identity && map.get(heldKey) === heldValue;
      identity = identity && heldMarker.name === "retained-marker";
      identity = identity && allocation.seed === heldKey.id;
      total += heldKey.id * 100 + heldValue.score;
    });
    return [total, identity];
  }

  function visitSet() {
    let total = 0;
    let identity = true;
    set.forEach(function (value, key, owner) {
      const heldValue = value;
      const heldOwner = owner;
      const heldMarker = marker;
      const allocation = churn(value.id + 100);
      identity = identity && heldOwner === set && heldValue === key;
      identity = identity && set.has(heldValue);
      identity = identity && heldMarker.version === 9;
      identity = identity && allocation.seed === heldValue.id + 100;
      total += heldValue.id;
    });
    return [total, identity];
  }

  for (let i = 0; i < 96; i++) {
    visitMap();
    visitSet();
  }
  return { probe: () => [visitMap(), visitSet()] };
})();
"#;

#[test]
fn map_and_set_callback_data_survives_allocating_reentry() {
    assert_interpreter_and_template(
        MAP_SET_DATA_SETUP,
        "JSON.stringify(__gcMapSetFixture.probe());",
        "gc-map-set-data",
        "[[660,true],[15,true]]",
    );
}

const MAP_COMPUTED_SETUP: &str = r#"
globalThis.__gcMapComputedFixture = (() => {
  const key = { id: 73 };
  const map = new Map();

  function compute(receivedKey) {
    const result = { receivedKey, marker: "computed-value" };
    let tail = null;
    for (let i = 0; i < 10; i++) {
      tail = { i, receivedKey, result, tail };
    }
    // The proposal requires the final write to overwrite a callback mutation.
    map.set(receivedKey, { marker: "callback-write" });
    result.tail = tail;
    return result;
  }

  function run() {
    map.delete(key);
    const result = map.getOrInsertComputed(key, compute);
    return [
      map.get(key) === result,
      result.receivedKey === key,
      result.marker,
      result.tail.result === result,
      result.tail.receivedKey === key
    ];
  }

  for (let i = 0; i < 96; i++) run();
  return { probe: run };
})();
"#;

#[test]
fn map_get_or_insert_computed_rereads_relocated_state() {
    assert_interpreter_and_template(
        MAP_COMPUTED_SETUP,
        "JSON.stringify(__gcMapComputedFixture.probe());",
        "gc-map-get-or-insert-computed",
        "[true,true,\"computed-value\",true,true]",
    );
}

const TYPED_ARRAY_SETUP: &str = r#"
globalThis.__gcTypedArrayFixture = (() => {
  const numbers = new Uint32Array([1, 2, 3, 4]);
  const hasBigIntArrays = typeof BigInt64Array === "function";
  const big = hasBigIntArrays ? new BigInt64Array([1n, 2n, 3n, 4n]) : null;

  function churn(seed) {
    let tail = null;
    for (let i = 0; i < 6; i++) tail = { seed, i, tail };
    return tail;
  }

  function reduceNumber(accumulator, value, index, owner) {
    const heldAccumulator = accumulator;
    const heldOwner = owner;
    const allocation = churn(value + index);
    heldAccumulator.sum += value;
    heldAccumulator.count++;
    heldAccumulator.ownerOk = heldAccumulator.ownerOk && heldOwner === numbers;
    heldAccumulator.allocationOk =
      heldAccumulator.allocationOk && allocation.seed === value + index;
    return heldAccumulator;
  }

  function findBig(value, index, owner) {
    const held = value;
    const allocation = churn(index + 20);
    return owner === big && allocation.seed === index + 20 && held === 3n;
  }

  function filterBig(value, index, owner) {
    const held = value;
    const allocation = churn(index + 40);
    return owner === big && allocation.seed === index + 40 && (held & 1n) === 0n;
  }

  function run() {
    const initial = { sum: 0, count: 0, ownerOk: true, allocationOk: true };
    const reduced = numbers.reduce(reduceNumber, initial);
    let found = "unsupported";
    let filtered = "unsupported";
    if (hasBigIntArrays) {
      found = big.find(findBig).toString();
      filtered = big.filter(filterBig).join(",");
    }
    return [
      reduced === initial,
      reduced.sum,
      reduced.count,
      reduced.ownerOk,
      reduced.allocationOk,
      hasBigIntArrays,
      found,
      filtered
    ];
  }

  for (let i = 0; i < 96; i++) run();
  return { probe: run };
})();
"#;

#[test]
fn typed_array_accumulators_and_bigint_callbacks_survive_gc() {
    assert_interpreter_and_template(
        TYPED_ARRAY_SETUP,
        "JSON.stringify(__gcTypedArrayFixture.probe());",
        "gc-typed-array-callbacks",
        "[true,10,4,true,true,true,\"3\",\"2,4\"]",
    );
}

const SORT_SETUP: &str = r#"
globalThis.__gcSortFixture = (() => {
  const a = { rank: 1, id: "a" };
  const b = { rank: 2, id: "b" };
  const c = { rank: 3, id: "c" };
  const d = { rank: 4, id: "d" };

  function comparator(left, right) {
    const heldLeft = left;
    const heldRight = right;
    let allocation = null;
    for (let i = 0; i < 8; i++) {
      allocation = { left: heldLeft.id, right: heldRight.id, i, allocation };
    }
    if (allocation.left !== heldLeft.id || allocation.right !== heldRight.id) return 999;
    return heldLeft.rank - heldRight.rank;
  }

  function run() {
    const mutable = [c, a, d, b];
    const sorted = mutable.sort(comparator);
    const source = [d, b, c, a];
    const copied = source.toSorted(comparator);
    return [
      sorted[0].id + sorted[1].id + sorted[2].id + sorted[3].id,
      copied[0].id + copied[1].id + copied[2].id + copied[3].id,
      source[0].id + source[1].id + source[2].id + source[3].id,
      sorted[0] === a && sorted[1] === b && sorted[2] === c && sorted[3] === d,
      copied[0] === a && copied[1] === b && copied[2] === c && copied[3] === d
    ];
  }

  for (let i = 0; i < 96; i++) run();
  return { probe: run };
})();
"#;

#[test]
fn sort_and_to_sorted_keep_object_operands_rooted() {
    assert_interpreter_and_template(
        SORT_SETUP,
        "JSON.stringify(__gcSortFixture.probe());",
        "gc-sort-comparator",
        "[\"abcd\",\"abcd\",\"dbca\",true,true]",
    );
}

const NATIVE_CONSTRUCT_SETUP: &str = r#"
globalThis.__gcNativeConstructFixture = (() => {
  class AlternateMapTarget {}
  const prototype = AlternateMapTarget.prototype;
  prototype.constructorMarker = "custom-new-target";

  function churn(seed) {
    let tail = null;
    for (let i = 0; i < 12; i++) tail = { seed, i, tail };
    return tail;
  }

  function construct(seed) {
    const left = { seed, side: "left" };
    const right = { seed: seed + 1, side: "right" };
    const result = Reflect.construct(Map, [], AlternateMapTarget);
    Map.prototype.set.call(result, left, right);
    const allocation = churn(seed + 1000);
    return [
      Object.getPrototypeOf(result) === prototype,
      Map.prototype.get.call(result, left) === right,
      Map.prototype.has.call(result, left),
      Map.prototype.get.call(result, left).seed === seed + 1,
      left.seed === seed,
      right.seed === seed + 1,
      allocation.seed === seed + 1000,
      Object.getPrototypeOf(result).constructorMarker === "custom-new-target"
    ];
  }

  for (let i = 0; i < 128; i++) construct(i);
  return { probe: () => construct(900) };
})();
"#;

#[test]
fn native_map_construct_keeps_this_and_new_target_rooted() {
    assert_interpreter_and_template(
        NATIVE_CONSTRUCT_SETUP,
        "JSON.stringify(__gcNativeConstructFixture.probe());",
        "gc-native-map-construct",
        "[true,true,true,true,true,true,true,true]",
    );
}
