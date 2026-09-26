//! Arrays grown through indexed stores keep the plain dense representation
//! generated element code reads.
//!
//! # Contents
//! - Reverse fills (within the dense gap) and appends complete as ordinary dense
//!   writes: the array keeps no descriptor sidecar, so a warm reader over it
//!   stays in generated element code afterwards.
//! - Int32 values stored into packed-double storage stay generated element
//!   hits in the optimizing tier.
//! - Every tier agrees on the stored values and their descriptors.
//!
//! # Invariants
//! - A default-attribute data element never records per-index flags.
//! - A warm element hit takes no runtime property stub.
//!
//! # See also
//! - `crates/otter-difftest/corpus/array_index_growth.js` compares the same
//!   paths with every observable exception.

use otter_runtime::{ExecutionResult, JitSelection, Runtime, RuntimeExecutionStats, SourceInput};

const TIERS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

const SETUP: &str = r#"
function reverseFill(n) {
    const out = new Array();
    let i = n;
    while (--i >= 0) out[i] = i & 3;
    return out;
}
function appendInts(n) {
    const out = [];
    for (let i = 0; i < n; i++) out[i] = (i * 7) & 0xff;
    return out;
}
function sum(values) {
    let total = 0;
    for (let i = 0; i < values.length; i++) total = (total + values[i]) | 0;
    return total;
}
function am(src, dst, n) {
    let c = 0;
    for (let i = 0; i < n; i++) {
        const l = src[i] * 3 + dst[i] + c;
        c = l >> 28;
        dst[i] = l & 0xfffffff;
    }
    return c;
}
globalThis.doubles = [0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5];
for (let warm = 0; warm < 400; warm++) {
    sum(reverseFill(32));
    sum(appendInts(32));
    am(appendInts(8), doubles, 8);
}
"#;

fn runtime(selection: JitSelection) -> Runtime {
    Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("array growth runtime")
}

fn run(runtime: &mut Runtime, source: &str) -> ExecutionResult {
    runtime
        .run_script(SourceInput::from_javascript(source), "array-growth.js")
        .unwrap_or_else(|error| panic!("array growth fixture: {error:?}"))
}

fn property_stubs(before: &RuntimeExecutionStats, after: &RuntimeExecutionStats) -> u64 {
    after.jit_runtime_property_stubs - before.jit_runtime_property_stubs
}

#[test]
fn grown_arrays_stay_on_generated_element_paths() {
    let mut runtime = runtime(JitSelection::ProductionTiered);
    run(&mut runtime, SETUP);
    run(
        &mut runtime,
        "globalThis.filled = reverseFill(1000); globalThis.appended = appendInts(1000);",
    );
    let before = runtime.execution_stats();
    let completion = run(&mut runtime, "sum(filled) + ',' + sum(appended)")
        .completion_string()
        .to_owned();
    let after = runtime.execution_stats();
    assert_eq!(completion, "1500,126516");
    assert_eq!(
        property_stubs(&before, &after),
        0,
        "reads over grown arrays must stay generated: before={before:?} after={after:?}"
    );

    let before = runtime.execution_stats();
    run(&mut runtime, "am(appendInts(8), doubles, 8)");
    let after = runtime.execution_stats();
    // Only `appendInts` grows its fresh array through the committed store.
    assert!(
        property_stubs(&before, &after) <= 8,
        "int32 stores into packed doubles must stay generated: before={before:?} after={after:?}"
    );
}

#[test]
fn every_tier_agrees_on_grown_arrays() {
    const PROBE: &str = r#"
const results = [];
const grown = reverseFill(5);
const d = Object.getOwnPropertyDescriptor(grown, "3");
results.push(grown.join(), d.writable && d.enumerable && d.configurable, Object.keys(grown).join());
const mixed = [0.5, 1.5];
am(appendInts(4), mixed, 4);
results.push(JSON.stringify(mixed), mixed.length);
JSON.stringify(results);
"#;
    let mut expected = None;
    for selection in TIERS {
        let mut runtime = runtime(selection);
        run(&mut runtime, SETUP);
        let actual = run(&mut runtime, PROBE).completion_string().to_owned();
        match &expected {
            None => expected = Some(actual),
            Some(expected) => assert_eq!(&actual, expected, "{selection:?}"),
        }
    }
    assert_eq!(
        expected.as_deref(),
        Some(r#"["0,1,2,3,0",true,"0,1,2,3,4","[0,22,0,0]",4]"#)
    );
}
