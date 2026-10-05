//! Optimizing-tier element-store execution, growth, and throw parity.
//!
//! # Contents
//! - Integer and float writes through a hot optimized store function.
//! - A float-array read-modify-write loop with unboxed arithmetic between
//!   element load/store transitions.
//! - Dense growth past the current length and a null-receiver throw.
//! - Interpreter-oracle comparison plus per-call optimized-execution evidence.
//!
//! # Invariants
//! - Interpreter and tiered runs execute identical source and warmup programs.
//! - Final store, read-modify-write, growth, and throw calls all enter already
//!   optimized functions in the tiered run. Optimizing generations count no
//!   entries, so a call proves it by entering no Template generation and
//!   charging fewer interpreter work units than the same call interpreted.
//! - Proven in-bounds primitive overwrites remain in optimized code.
//!   Growth and invalid receivers leave through one exact pre-effect deopt and
//!   resume the canonical interpreter operation without replay.
//!
//! # See also
//! - `crates/otter-difftest/corpus/arrays_typed.js` exercises the same float
//!   read-modify-write shape across the moving-GC stride matrix.

use otter_runtime::{JitSelection, Runtime, SourceInput};

/// Counter deltas of one final call.
#[derive(Clone, Copy, Debug)]
struct CallDelta {
    work_units: u64,
    template_entries: u64,
    optimized_deopts: u64,
    code_generations: u64,
}

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

    // A compiled hot caller reaches each callee through generated linkage;
    // interpreted straight-line calls never repay a native entry transition
    // for bodies this small. The optimizing tier does not inline calls inside
    // a try region, so each callee is promoted to its own optimized
    // generation instead of only running inside this caller.
    function warmStores() {
      for (let warm = 0; warm < 4010; warm++) {
        try {
          hotStoreZero(storeInts, 7);
          hotStoreZero(storeFloats, 1.25);
          hotFloatRmw(rmwA, rmwB, 0.5);
          hotGrowEight(growWarm, 9.5);
        } finally {}
      }
    }
    warmStores();
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

fn run(selection: JitSelection) -> (String, Vec<CallDelta>) {
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
        deltas.push(CallDelta {
            work_units: after.work_units_executed - before.work_units_executed,
            template_entries: after.jit_generated_template_entries
                - before.jit_generated_template_entries,
            optimized_deopts: after.jit_optimized_deopts - before.jit_optimized_deopts,
            code_generations: after.jit_code_generations - before.jit_code_generations,
        });
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
    let (oracle, interpreted) = run(JitSelection::InterpreterOnly);
    let (tiered, deltas) = run(JitSelection::ProductionTiered);

    assert_eq!(tiered, oracle);
    assert_eq!(
        oracle,
        r#"{"intResult":17,"intContents":17,"floatResult":2.75,"floatContents":2.75,"rmwResult":10,"rmwContents":[1,2,3,4],"growResult":42.5,"growLength":9,"growFirstPresent":false,"growStoredPresent":true,"throwName":"TypeError"}"#
    );
    assert_eq!(deltas.len(), 5);
    for ((operation, expected_deopts), (delta, interpreted)) in [
        ("int", 0),
        ("float", 0),
        ("read-modify-write", 0),
        // The out-of-bounds append leaves the speculative store once before
        // any effect; the interpreter completes it and the site commits.
        ("growth", 1),
        // A null receiver leaves the speculative store once; the interpreter
        // throws the canonical TypeError.
        ("throw", 1),
    ]
    .into_iter()
    .zip(deltas.into_iter().zip(interpreted))
    {
        assert!(
            delta.template_entries == 0 && delta.work_units < interpreted.work_units,
            "{operation} store must enter optimized code: {delta:?}, interpreted {interpreted:?}"
        );
        assert_eq!(
            delta.optimized_deopts, expected_deopts,
            "{operation} store must follow its exact generated/deopt contract: {delta:?}"
        );
        if expected_deopts == 0 {
            assert_eq!(delta.code_generations, 0, "{operation}: {delta:?}");
        }
    }
}
