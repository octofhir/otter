//! Optimizing-tier element-store execution, growth, and throw parity.
//!
//! # Contents
//! - Integer and float writes through a hot optimized store function.
//! - A float-array read-modify-write loop with unboxed arithmetic between
//!   element load/store transitions.
//! - Dense growth past the current length and a null-receiver throw.
//! - Interpreter-oracle comparison plus per-call optimized-entry evidence.
//!
//! # Invariants
//! - Interpreter and tiered runs execute identical source and warmup programs.
//! - Final store, read-modify-write, growth, and throw calls all enter already
//!   optimized functions in the tiered run.
//! - Array mutation remains owned by `STUB_JIT_STORE_ELEMENT`; optimized code
//!   delegates through the canonical active frame without materializing an
//!   interpreter activation.
//!
//! # See also
//! - `crates/otter-difftest/corpus/arrays_typed.js` exercises the same float
//!   read-modify-write shape across the moving-GC stride matrix.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SETUP: &str = r#"
    function hotStoreZero(values, value) {
      values[0] = value;
      return values[0];
    }

    function hotFloatRmw(a, b, c) {
      for (let i = 0; i < 4; i = i + 1) {
        a[i] = a[i] * c + b[i];
      }
      return a[0] + a[1] + a[2] + a[3];
    }

    function hotGrowEight(values, value) {
      values[8] = value;
      return values[8];
    }

    globalThis.storeInts = [1, 2];
    globalThis.storeFloats = [0.5, 1.5];
    globalThis.rmwA = [1.25, 2.5, 3.75, 5];
    globalThis.rmwB = [0.5, 1, 1.5, 2];
    globalThis.growWarm = [0, 1, 2, 3, 4, 5, 6, 7, 8];

    let warmStores = "";
    for (let warm = 0; warm < 4010; warm++) {
      warmStores += "hotStoreZero(storeInts, 7);";
      warmStores += "hotStoreZero(storeFloats, 1.25);";
      warmStores += "hotFloatRmw(rmwA, rmwB, 0.5);";
      warmStores += "hotGrowEight(growWarm, 9.5);";
    }
    eval(warmStores);
"#;

const FINAL_CALLS: [(&str, &str); 5] = [
    (
        "globalThis.intResult = hotStoreZero(storeInts, 17);",
        "optimizing-store-element-int.js",
    ),
    (
        "globalThis.floatResult = hotStoreZero(storeFloats, 2.75);",
        "optimizing-store-element-float.js",
    ),
    (
        "globalThis.rmwResult = hotFloatRmw(rmwA, rmwB, 0.5);",
        "optimizing-store-element-rmw.js",
    ),
    (
        "globalThis.grown = []; globalThis.growResult = hotGrowEight(grown, 42.5);",
        "optimizing-store-element-grow.js",
    ),
    (
        r#"globalThis.throwName = "";
           try { hotStoreZero(null, 1); } catch (error) { throwName = error.name; }"#,
        "optimizing-store-element-throw.js",
    ),
];

const OBSERVE: &str = r#"
    JSON.stringify({
      intResult,
      intContents: storeInts[0],
      floatResult,
      floatContents: storeFloats[0],
      rmwResult,
      rmwContents: rmwA,
      growResult,
      growLength: grown.length,
      growFirstPresent: 0 in grown,
      growStoredPresent: 8 in grown,
      throwName
    });
"#;

fn run(selection: JitSelection) -> (String, Vec<(u64, u64)>) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(SETUP),
            "optimizing-store-element-setup.js",
        )
        .expect("store warmup");
    let mut deltas = Vec::new();
    for (source, url) in FINAL_CALLS {
        let before = runtime.execution_stats();
        runtime
            .run_script(SourceInput::from_javascript(source), url)
            .expect("final store call");
        let after = runtime.execution_stats();
        deltas.push((
            after.jit_optimized_entries - before.jit_optimized_entries,
            after.jit_optimized_deopts - before.jit_optimized_deopts,
        ));
    }
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(OBSERVE),
            "optimizing-store-element-observe.js",
        )
        .expect("observe store results")
        .completion_string()
        .to_owned();
    (completion, deltas)
}

#[test]
fn optimized_element_stores_match_interpreter() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (tiered, deltas) = run(JitSelection::Baseline);

    assert_eq!(tiered, oracle);
    assert_eq!(
        oracle,
        r#"{"intResult":17,"intContents":17,"floatResult":2.75,"floatContents":2.75,"rmwResult":10,"rmwContents":[1,2,3,4],"growResult":42.5,"growLength":9,"growFirstPresent":false,"growStoredPresent":true,"throwName":"TypeError"}"#
    );
    assert_eq!(deltas.len(), 5);
    for (operation, (optimized_entries, optimized_deopts)) in
        ["int", "float", "read-modify-write", "growth", "throw"]
            .into_iter()
            .zip(deltas)
    {
        assert!(
            optimized_entries >= 1,
            "{operation} store must enter optimized code: entries={optimized_entries}, deopts={optimized_deopts}"
        );
        assert_eq!(
            optimized_deopts, 0,
            "{operation} store must not deopt on supported values"
        );
    }
}
