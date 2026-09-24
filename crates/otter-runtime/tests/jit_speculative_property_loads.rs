//! Speculative monomorphic own-data loads in the optimizing tier.
//!
//! # Contents
//! - A warmed site lowers to a shape proof, one exact exit, and a slot read;
//!   a loop-invariant receiver's proof leaves the loop, including when the loop
//!   is entered by OSR.
//! - A receiver of another shape exits once, and the recompiled site keeps
//!   the committed probe instead of repeating the speculation.
//! - An accessor installed over the slot runs its getter exactly once per read.
//! - A loop that rewrites the receiver's shape keeps the proof inside the loop.
//! - A callee spliced into its caller exits at its own site.
//!
//! # Invariants
//! - Results match the interpreter oracle exactly.
//! - A failed speculation costs a bounded number of exits, never one per call.

use otter_runtime::{
    JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    RuntimeExecutionStats, SourceInput,
};

struct Run {
    completion: String,
    stats: RuntimeExecutionStats,
    proofs: usize,
    hoisted: bool,
}

fn run(source: &str, selection: JitSelection, function: &str) -> Run {
    let mut builder = Runtime::builder().jit_selection(selection);
    if selection == JitSelection::ProductionTiered {
        builder = builder.jit_debug(JitDebugRequest::artifacts());
    }
    let mut runtime = builder.build().expect("speculative load runtime");
    let result = runtime
        .run_script(SourceInput::from_javascript(source), "speculative-load.js")
        .unwrap_or_else(|error| panic!("speculative load fixture: {error:?}"));
    let completion = result.completion_string().to_owned();
    let mut proofs = 0;
    let mut hoisted = false;
    if let Some(artifacts) = result.jit_artifacts() {
        for bundle in artifacts.bundles() {
            let manifest = bundle.manifest();
            if manifest.function_name() != function || manifest.tier() != JitDebugTier::Optimizing {
                continue;
            }
            let code_map = bundle
                .file(JitArtifactFileName::CodeMap)
                .expect("optimizing code map")
                .contents();
            proofs = proofs.max(
                String::from_utf8_lossy(code_map)
                    .matches("\"machinePropertyShapeProof\"")
                    .count(),
            );
            let ir = bundle
                .file(JitArtifactFileName::OptimizedIr)
                .expect("optimized IR")
                .contents();
            hoisted |= String::from_utf8_lossy(ir).lines().any(|line| {
                line.starts_with("; licm-hoisted=") && !line.starts_with("; licm-hoisted=0 ")
            });
        }
    }
    Run {
        completion,
        stats: runtime.execution_stats(),
        proofs,
        hoisted,
    }
}

fn compare(source: &str, function: &str, expected: &str) -> Run {
    let oracle = run(source, JitSelection::InterpreterOnly, function);
    assert_eq!(oracle.completion, expected);
    let compiled = run(source, JitSelection::ProductionTiered, function);
    assert_eq!(compiled.completion, oracle.completion);
    compiled
}

fn optimized_entries(stats: &RuntimeExecutionStats) -> u64 {
    stats.jit_optimized_entries
        + stats.jit_optimized_osr_entries
        + stats.jit_generated_optimizing_entries
}

#[test]
fn warmed_site_is_shape_proven_and_its_proof_leaves_the_loop() {
    const SOURCE: &str = r#"
const box = { value: 0.5, scale: 8 };
function scaled(count) {
    const local = box;
    let total = 0;
    for (let index = 0; index < count; index++) total += local.value * local.scale;
    return total;
}
let checksum = 0;
for (let call = 0; call < 300; call++) checksum += scaled(100);
checksum += scaled(200000);
String(checksum);
"#;
    let run = compare(SOURCE, "scaled", "920000");
    assert!(run.proofs >= 1, "the warmed load must be shape-proven");
    assert!(
        run.hoisted,
        "the invariant receiver proof must leave the loop"
    );
    assert!(optimized_entries(&run.stats) >= 8, "{:?}", run.stats);
    assert_eq!(run.stats.jit_optimized_deopts, 0, "{:?}", run.stats);
    assert_eq!(run.stats.jit_runtime_property_stubs, 0, "{:?}", run.stats);
}

#[test]
fn osr_entered_loop_performs_the_hoisted_proof() {
    const SOURCE: &str = r#"
const box = { value: 3, scale: 2 };
function once(count) {
    const local = box;
    let total = 0;
    for (let index = 0; index < count; index++) total += local.value * local.scale + (index & 1);
    return total;
}
String(once(400000));
"#;
    let run = compare(SOURCE, "once", "2600000");
    assert!(run.stats.jit_optimized_osr_entries >= 1, "{:?}", run.stats);
    assert!(
        run.proofs >= 1 && run.hoisted,
        "the OSR unit must hoist the proof"
    );
    assert_eq!(run.stats.jit_optimized_deopts, 0, "{:?}", run.stats);
}

#[test]
fn another_shape_exits_once_and_the_site_keeps_the_committed_probe() {
    const SOURCE: &str = r#"
function read(record) {
    let total = 0;
    for (let index = 0; index < 64; index++) total += record.value;
    return total;
}
const first = { value: 1 };
for (let warm = 0; warm < 400; warm++) read(first);
const second = { padding: 0, value: 2 };
let checksum = 0;
for (let call = 0; call < 400; call++) checksum += read(second) + read(first);
String(checksum);
"#;
    let run = compare(SOURCE, "read", "76800");
    assert!(run.proofs >= 1, "the first generation must speculate");
    assert!(optimized_entries(&run.stats) >= 400, "{:?}", run.stats);
    assert!(
        run.stats.jit_optimized_deopts <= 2,
        "a failed shape speculation must not exit per call: {:?}",
        run.stats
    );
}

#[test]
fn accessor_over_the_slot_runs_its_getter_once_per_read() {
    const SOURCE: &str = r#"
function read(record) { return record.value; }
const record = { value: 1 };
let warmed = 0;
for (let warm = 0; warm < 2000; warm++) warmed += read(record);
let calls = 0;
Object.defineProperty(record, "value", { get() { calls++; return 5; } });
let total = 0;
for (let call = 0; call < 500; call++) total += read(record);
JSON.stringify([warmed, total, calls]);
"#;
    let run = compare(SOURCE, "read", "[2000,2500,500]");
    assert!(run.stats.jit_optimized_deopts <= 2, "{:?}", run.stats);
}

#[test]
fn loop_rewriting_the_receiver_shape_keeps_the_proof_inside() {
    const SOURCE: &str = r#"
function churn(record, count) {
    let total = 0;
    for (let index = 0; index < count; index++) {
        total += record.value;
        if (index === count - 3) {
            delete record.value;
            record.extra = 1;
            record.value = 100;
        }
    }
    return total;
}
let checksum = 0;
for (let call = 0; call < 300; call++) checksum += churn({ value: 1 }, 64);
String(checksum);
"#;
    compare(SOURCE, "churn", "78600");
}

#[test]
fn inlined_callee_exits_at_its_own_site() {
    const SOURCE: &str = r#"
const receiver = {
    bias: 4,
    apply(value) { return value + this.bias; },
};
function drive(target, count) {
    let total = 0;
    for (let index = 0; index < count; index++) total += target.apply(index);
    return total;
}
let checksum = 0;
for (let call = 0; call < 300; call++) checksum += drive(receiver, 64);
const moved = { extra: 1, bias: 7, apply: receiver.apply };
for (let call = 0; call < 300; call++) checksum += drive(moved, 64) + drive(receiver, 64);
String(checksum);
"#;
    let run = compare(SOURCE, "drive", "2102400");
    assert!(optimized_entries(&run.stats) >= 300, "{:?}", run.stats);
    assert!(run.stats.jit_optimized_deopts <= 4, "{:?}", run.stats);
}
