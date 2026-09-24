//! Declared leaves at explicit-receiver calls whose callee is already loaded.
//!
//! # Contents
//! - Warm `s.charCodeAt(i)`, `s.indexOf(n)`, `map.get(k)`, `set.has(v)` and
//!   tagged `Math` calls complete through declared leaves without crossings.
//! - Replaced callees, foreign or wrapped receivers, out-of-range and
//!   non-Int32 indices, rope keys and Map subclasses keep interpreter results.
//! - `this`-reading declarations are never lowered at plain calls or shaped
//!   method sites, which pass no receiver word.
//! - Moving collection between leaf hits preserves receivers and results.
//! - `map.set` overwrites existing keys through the in-place leaf; inserts,
//!   replaced callees and non-Map receivers take the ordinary call once, and
//!   young values stored into an old Map stay reachable.
//!
//! # Invariants
//! - The callee is guarded after argument evaluation; lookup never repeats.
//! - Every leaf miss commits one ordinary call; no tier deoptimizes.
//!
//! # See also
//! - `jit_machine_string_method_leaves` covers `CallMethodValue` leaf probes.
//! - `jit_machine_native_call_with_this` covers Int32 Math replacements.

use otter_runtime::{JitSelection, Runtime, RuntimeExecutionStats, SourceInput};

const TIERS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

const SETUP: &str = r#"
const originalCharCodeAt = String.prototype.charCodeAt;
const originalMapGet = Map.prototype.get;
const table = new Map([[1, 10], [2, 20], ["key", 30]]);
const members = new Set([3, "member"]);
function codes(text, count) {
    let total = 0;
    for (let index = 0; index < count; index++) total += text.charCodeAt(index & 3);
    return total;
}
function finds(text, needle, count) {
    let total = 0;
    for (let index = 0; index < count; index++) total += text.indexOf(needle);
    return total;
}
function gets(map, count) {
    let total = 0;
    for (let index = 0; index < count; index++) total += map.get((index & 1) + 1);
    return total;
}
function hasMembers(set, count) {
    let total = 0;
    for (let index = 0; index < count; index++) if (set.has(index & 3)) total++;
    return total;
}
function magnitudes(scale, count) {
    let total = 0;
    for (let index = 0; index < count; index++) {
        total += Math.abs((index & 3) * scale - scale) + Math.max((index & 1) * scale, scale / 2);
    }
    return total;
}
function stores(map, count) {
    let total = 0;
    for (let index = 0; index < count; index++) {
        total += map.set((index & 1) + 1, index).size;
    }
    return total;
}
const writable = new Map([[1, 0], [2, 0]]);
function join(left, right) { return left + right; }
for (let warm = 0; warm < 40; warm++) {
    stores(writable, 200);
    codes("otter", 200);
    finds("otter engine", "e", 200);
    gets(table, 200);
    hasMembers(members, 200);
    magnitudes(0.5, 200);
}
"#;

fn warmed(selection: JitSelection) -> Runtime {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("resolved leaf runtime");
    completion(&mut runtime, SETUP);
    runtime
}

fn completion(runtime: &mut Runtime, source: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), "resolved-leaves.js")
        .unwrap_or_else(|error| panic!("resolved leaf fixture: {error:?}"))
        .completion_string()
        .to_owned()
}

/// Whether this tier lowers explicit-receiver leaves on this target. The
/// x86-64 Template tier keeps its general call for explicit receivers.
fn generates_hit(selection: JitSelection) -> bool {
    match selection {
        JitSelection::InterpreterOnly => false,
        JitSelection::Template => cfg!(target_arch = "aarch64"),
        _ => true,
    }
}

fn assert_no_deopt(before: RuntimeExecutionStats, after: RuntimeExecutionStats) {
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(
        after.jit_generated_call_deopts,
        before.jit_generated_call_deopts
    );
}

#[test]
fn warm_explicit_receiver_leaves_do_not_cross_the_native_boundary() {
    for selection in TIERS {
        let mut runtime = warmed(selection);
        for (call, expected) in [
            ("codes('otter', 1000)", "111000"),
            ("finds('otter engine', 'e', 1000)", "3000"),
            ("gets(table, 1000)", "15000"),
            ("hasMembers(members, 1000)", "250"),
            ("magnitudes(0.5, 1000)", "875"),
            ("stores(writable, 1000) + writable.get(2)", "2999"),
        ] {
            let before = runtime.execution_stats();
            assert_eq!(
                completion(&mut runtime, call),
                expected,
                "{selection:?} {call}"
            );
            let after = runtime.execution_stats();
            assert_no_deopt(before, after);
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
fn misses_and_replacements_match_the_interpreter_once() {
    const MISSES: &str = r#"
const log = [];
log.push(codes("ab", 4));
log.push(Number.isNaN(codes("", 1)));
log.push(codes(join("x".repeat(40), "yz".repeat(30)), 4));
log.push(codes(new String("otter"), 4));
let replaced = 0;
String.prototype.charCodeAt = function (index) { replaced++; return 1000 + index; };
log.push(codes("otter", 4), replaced);
String.prototype.charCodeAt = originalCharCodeAt;
log.push(finds("otter", { toString() { return "t"; } }, 2));
log.push(gets(new Map([[1, "a"], [2, "b"]]), 2));
class CountingMap extends Map {}
log.push(gets(new CountingMap([[1, 4], [2, 5]]), 2));
log.push(gets(new Map([[1, 1]]), 2));
let mapGets = 0;
Map.prototype.get = function (key) { mapGets++; return 2; };
log.push(gets(table, 3), mapGets);
Map.prototype.get = originalMapGet;
try {
    log.push(gets({ get: undefined }, 1));
} catch (error) {
    log.push(error.constructor.name);
}
try {
    log.push(Reflect.apply(gets, undefined, [Object.create(Map.prototype), 1]));
} catch (error) {
    log.push(error.constructor.name);
}
log.push(magnitudes(3, 4), magnitudes(0x7fffffff, 2));
const plainCharCodeAt = String.prototype.charCodeAt;
function plain(count) {
    let names = "";
    for (let index = 0; index < count; index++) {
        try { plainCharCodeAt(0); } catch (error) { names = error.constructor.name; }
    }
    return names;
}
log.push(plain(300));
const holder = { code: String.prototype.charCodeAt };
function shaped(count) {
    let total = 0;
    for (let index = 0; index < count; index++) total += holder.code(0);
    return total;
}
log.push(shaped(300));
JSON.stringify(log);
"#;
    let mut oracle = warmed(JitSelection::InterpreterOnly);
    let expected = completion(&mut oracle, MISSES);
    assert_eq!(
        expected,
        r#"[null,true,480,444,4006,4,2,"0ab",9,null,6,3,"TypeError","TypeError",21,5368709117.5,"TypeError",27300]"#
    );
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = warmed(selection);
        assert_eq!(completion(&mut runtime, MISSES), expected, "{selection:?}");
    }
}

#[test]
fn numeric_leaf_misses_commit_without_deoptimizing() {
    // Every call below misses its leaf (wrapper or rope receiver, replaced
    // callee, subclass instance, non-member) while keeping numeric results,
    // so no arithmetic speculation can explain a deopt.
    const LEAF_MISSES: &str = r#"
const log = [];
log.push(codes(new String("otter"), 400));
log.push(codes(join("x".repeat(40), "yz".repeat(30)), 400));
String.prototype.charCodeAt = function (index) { return 7; };
log.push(codes("otter", 400));
String.prototype.charCodeAt = originalCharCodeAt;
class CountingMap extends Map {}
log.push(gets(new CountingMap([[1, 4], [2, 5]]), 400));
log.push(hasMembers(new Set(["x"]), 400));
log.push(finds(join("q".repeat(64), "needle"), "needle", 400));
JSON.stringify(log);
"#;
    let mut oracle = warmed(JitSelection::InterpreterOnly);
    let expected = completion(&mut oracle, LEAF_MISSES);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = warmed(selection);
        let before = runtime.execution_stats();
        assert_eq!(
            completion(&mut runtime, LEAF_MISSES),
            expected,
            "{selection:?}"
        );
        assert_no_deopt(before, runtime.execution_stats());
    }
}

#[test]
fn moving_collection_between_leaf_hits_preserves_receivers() {
    const CHURN: &str = r#"
function churn(count) {
    const kept = [];
    let total = 0;
    for (let index = 0; index < count; index++) {
        const word = "engine-" + (index * 7919 + 100003);
        const map = new Map([[index, word]]);
        kept.push(map);
        total += word.charCodeAt(index & 7) + kept[index >> 1].get(index >> 1).indexOf("-");
        total += members.has(index & 3) ? 1 : 0;
    }
    return total;
}
for (let warm = 0; warm < 40; warm++) churn(64);
churn(512);
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
fn in_place_map_set_misses_on_insert_and_keeps_young_values() {
    const SETS: &str = r#"
const log = [];
const fresh = new Map([[1, 0]]);
log.push(stores(fresh, 4), fresh.size, fresh.get(2));
const originalMapSet = Map.prototype.set;
let replacedSets = 0;
Map.prototype.set = function (key, value) { replacedSets++; return this; };
log.push(stores(writable, 3), replacedSets);
Map.prototype.set = originalMapSet;
try { log.push(Reflect.apply(stores, undefined, [new Set([1]), 1])); } catch (error) { log.push(error.constructor.name); }
function youngValues(map, count) {
    for (let index = 0; index < count; index++) {
        map.set((index & 1) + 1, { word: "young-" + (index * 7919 + 100003) });
    }
    return map.get(1).word + map.get(2).word;
}
log.push(youngValues(writable, 257));
JSON.stringify(log);
"#;
    let mut oracle = warmed(JitSelection::InterpreterOnly);
    let expected = completion(&mut oracle, SETS);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = warmed(selection);
        runtime
            .force_gc()
            .expect("promote the warmed Map before young stores");
        assert_eq!(completion(&mut runtime, SETS), expected, "{selection:?}");
    }
}
