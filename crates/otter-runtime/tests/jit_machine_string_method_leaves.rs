//! Generated leaf hits for guarded String and collection method calls.
//!
//! # Contents
//! - Warm `String.prototype.indexOf` and `Map.prototype.get` sites complete
//!   through their declared no-allocation leaves without method transitions.
//! - `%String.prototype%` layout changes, replacement, accessors, coercion,
//!   wrapper receivers and ropes keep interpreter semantics.
//! - A deleted method whose slot was reused by another key throws exactly as
//!   the interpreter does in every tier.
//! - Moving collection between leaf hits preserves every receiver and result.
//! - An own `get` assigned to a Map allocates its expando bag without losing
//!   the collection, value or receiver across that allocation.
//! - `indexOf` / `lastIndexOf` misses keep a fresh search string rooted while
//!   a rope receiver flattens.
//!
//! # Invariants
//! - A proof or leaf miss commits one canonical method call; no tier deopts.
//! - A dictionary-mode holder is trusted only under its captured layout id.
//! - A leaf hit neither allocates nor publishes a runtime transition.
//!
//! # See also
//! - `jit_machine_builtin_properties` covers generated collection property loads.
//! - `optimizing_string_intrinsics` covers the string fallback matrix.

use otter_runtime::{
    ExecutionResult, JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    RuntimeExecutionStats, SourceInput,
};

const TIERS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

const SETUP: &str = r#"
const originalIndexOf = String.prototype.indexOf;
const originalMapGet = Map.prototype.get;
const counted = new Map([['k', 5]]);
function stringFind(receiver, count) {
    let total = 0;
    for (let index = 0; index < count; index++) total += receiver.indexOf("e");
    return total;
}
function mapFind(map, count) {
    let total = 0;
    for (let index = 0; index < count; index++) total += map.get("k");
    return total;
}
function join(left, right) { return left + right; }
for (let warm = 0; warm < 40; warm++) {
    stringFind("otter engine", 200);
    mapFind(counted, 200);
}
"#;

fn runtime(selection: JitSelection) -> Runtime {
    Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("string method leaf runtime")
}

fn run(runtime: &mut Runtime, source: &str) -> ExecutionResult {
    runtime
        .run_script(SourceInput::from_javascript(source), "string-leaves.js")
        .unwrap_or_else(|error| panic!("string method leaf fixture: {error:?}"))
}

fn completion(runtime: &mut Runtime, source: &str) -> String {
    run(runtime, source).completion_string().to_owned()
}

fn warmed(selection: JitSelection) -> Runtime {
    let mut runtime = runtime(selection);
    let result = run(&mut runtime, SETUP);
    if selection == JitSelection::ProductionTiered {
        for (name, holder) in [
            ("stringFind", "machineCacheIrGuardDictionaryLayout"),
            ("mapFind", "machineCacheIrGuardShape"),
        ] {
            let bundle = result
                .jit_artifacts()
                .expect("leaf artifacts")
                .bundles()
                .iter()
                .find(|bundle| {
                    bundle.manifest().function_name() == name
                        && bundle.manifest().tier() == JitDebugTier::Optimizing
                })
                .unwrap_or_else(|| panic!("{name} must optimize: {:?}", result.jit_debug_report()));
            let code_map = std::str::from_utf8(
                bundle
                    .file(JitArtifactFileName::CodeMap)
                    .expect("leaf code map")
                    .contents(),
            )
            .expect("UTF-8 code map");
            for region in [
                "machineCacheIrLoadIntrinsicPrototype",
                holder,
                "machineNativeLeafIdentity",
                "machineNativeLeafProbe",
            ] {
                assert!(
                    code_map.contains(region),
                    "{name} lacks {region}: {code_map}"
                );
            }
        }
    }
    runtime
}

/// Whether this tier generates the guarded leaf hit on this target. The x86-64
/// Template tier keeps its general method call for every guarded builtin.
fn generates_hit(selection: JitSelection) -> bool {
    match selection {
        JitSelection::InterpreterOnly => false,
        JitSelection::Template => cfg!(target_arch = "aarch64"),
        _ => true,
    }
}

fn assert_stable(before: RuntimeExecutionStats, after: RuntimeExecutionStats) {
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(
        after.jit_generated_call_deopts,
        before.jit_generated_call_deopts
    );
    assert_eq!(after.jit_code_generations, before.jit_code_generations);
}

#[test]
fn warm_leaf_hits_skip_the_general_method_boundary() {
    for selection in TIERS {
        let mut runtime = warmed(selection);
        for (call, expected) in [
            ("stringFind('otter engine', 1000)", "3000"),
            ("mapFind(counted, 1000)", "5000"),
        ] {
            let before = runtime.execution_stats();
            assert_eq!(completion(&mut runtime, call), expected, "{selection:?}");
            let after = runtime.execution_stats();
            assert_stable(before, after);
            if selection == JitSelection::ProductionTiered {
                assert!(after.jit_optimized_entries > before.jit_optimized_entries);
            }
            if generates_hit(selection) {
                assert_eq!(
                    after.jit_to_rust_call_transitions - before.jit_to_rust_call_transitions,
                    0,
                    "{selection:?} {call}: every warm call must stay a leaf hit"
                );
            }
        }
    }
}

#[test]
fn prototype_mutations_and_misses_match_the_interpreter_once() {
    const MUTATIONS: &str = r#"
const log = [];
log.push(stringFind("otter engine", 3));
String.prototype.unrelatedLeafKey = 1;
log.push(stringFind("otter engine", 3));
delete String.prototype.unrelatedLeafKey;
log.push(stringFind("otter engine", 3));
let replacedCalls = 0;
String.prototype.indexOf = function (needle) { replacedCalls++; return 100; };
log.push(stringFind("otter", 3), replacedCalls);
String.prototype.indexOf = originalIndexOf;
let getterReads = 0;
Object.defineProperty(String.prototype, "indexOf", {
    configurable: true,
    get() { getterReads++; return originalIndexOf; }
});
log.push(stringFind("otter", 3), getterReads);
Object.defineProperty(String.prototype, "indexOf", {
    configurable: true, writable: true, enumerable: false, value: originalIndexOf
});
log.push(stringFind("otter engine", 3));
log.push(stringFind(new String("engine"), 2));
log.push(stringFind(join("otter ", "engine"), 2));
log.push(stringFind("ωmega-e", 2));
let mapGets = 0;
Map.prototype.get = function (key) { mapGets++; return 7; };
log.push(mapFind(counted, 3), mapGets);
Map.prototype.get = originalMapGet;
const shadowed = new Map([['k', 5]]);
shadowed.get = function () { return 11; };
log.push(mapFind(shadowed, 2), mapFind(counted, 2));
const names = Object.getOwnPropertyNames(String.prototype);
const next = names[names.indexOf("indexOf") + 1];
String.prototype[next] = originalIndexOf;
delete String.prototype.indexOf;
try {
    log.push(stringFind("otter engine", 1));
} catch (error) {
    log.push(error.constructor.name);
}
JSON.stringify(log);
"#;
    let mut oracle = warmed(JitSelection::InterpreterOnly);
    let expected = completion(&mut oracle, MUTATIONS);
    assert_eq!(
        expected,
        r#"[9,9,9,300,3,9,3,9,0,6,4,21,3,22,10,"TypeError"]"#
    );
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = warmed(selection);
        let before = runtime.execution_stats();
        assert_eq!(
            completion(&mut runtime, MUTATIONS),
            expected,
            "{selection:?}"
        );
        assert_eq!(
            runtime.execution_stats().jit_optimized_deopts,
            before.jit_optimized_deopts,
            "{selection:?}: every miss must commit, not deoptimize"
        );
    }
}

#[test]
fn moving_collection_between_leaf_hits_preserves_receivers() {
    const CHURN: &str = r#"
function churn(count) {
    let total = 0;
    const kept = [];
    for (let index = 0; index < count; index++) {
        const word = "engine-" + index;
        kept.push({ word });
        total += word.indexOf("e") + kept[index >> 1].word.indexOf("-");
        total += counted.get("k");
    }
    return total;
}
for (let warm = 0; warm < 40; warm++) churn(64);
[churn(512), stringFind("otter engine", 64), mapFind(counted, 64)].join();
"#;
    let mut oracle = warmed(JitSelection::InterpreterOnly);
    let expected = completion(&mut oracle, CHURN);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = warmed(selection);
        let before = runtime.execution_stats();
        assert_eq!(completion(&mut runtime, CHURN), expected, "{selection:?}");
        let after = runtime.execution_stats();
        assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
        if let Some(stride @ (1 | 4 | 16)) = std::env::var("OTTER_GC_STRESS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            assert!(after.gc_minor_cycles - before.gc_minor_cycles >= 128 / stride);
            assert!(after.gc_minor_slot_updates > before.gc_minor_slot_updates);
        }
    }
}

#[test]
fn search_misses_root_fresh_needles_while_ropes_flatten() {
    const ROPES: &str = r#"
function searchRopes(count) {
    const results = [];
    for (let index = 0; index < count; index++) {
        const needle = (index * 7919 + 100003).toString();
        const haystack = join("abc".repeat(40), join(needle, "tail".repeat(20)));
        results.push(haystack.indexOf(needle), haystack.lastIndexOf(needle));
    }
    return results.join();
}
for (let warm = 0; warm < 40; warm++) searchRopes(8);
searchRopes(256);
"#;
    let mut oracle = warmed(JitSelection::InterpreterOnly);
    let expected = completion(&mut oracle, ROPES);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = warmed(selection);
        assert_eq!(completion(&mut runtime, ROPES), expected, "{selection:?}");
    }
}
