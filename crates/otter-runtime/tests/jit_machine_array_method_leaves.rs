//! Generated leaf hits for dense-array `push` / `pop` / `shift` calls.
//!
//! # Contents
//! - Warm `push` / `pop` / `shift` sites in the optimizing tier complete
//!   through their declared in-place leaves, with the array-index accessor
//!   protector guard ahead of `push`.
//! - A full buffer, a holey tail and a frozen receiver miss into the canonical
//!   call and match the interpreter.
//! - An indexed accessor installed on `%Array.prototype%` after warm-up trips
//!   the protector: `push` then reaches the inherited setter exactly as the
//!   interpreter does, in every tier.
//!
//! # Invariants
//! - A proof or leaf miss commits one canonical method call; no tier deopts.
//! - A leaf hit neither grows the buffer nor publishes a runtime transition.
//!
//! # See also
//! - `jit_machine_string_method_leaves` covers the String and collection
//!   leaves sharing the same Machine hit.

use otter_runtime::{
    ExecutionResult, JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    SourceInput,
};

const TIERS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

const SETUP: &str = r#"
function Queue() { this.items = []; }
Queue.prototype.add = function (value) { return this.items.push(value); };
Queue.prototype.removeLast = function () { return this.items.pop(); };
Queue.prototype.removeFirst = function () { return this.items.shift(); };
function churn(queue, count) {
    let total = 0;
    for (let index = 0; index < count; index++) {
        const next = index + 1;
        queue.add(index);
        queue.add(next);
        total += queue.removeLast();
        total += queue.removeFirst();
    }
    return total;
}
globalThis.queue = new Queue();
for (let index = 0; index < 64; index++) queue.add(0);
for (let index = 0; index < 64; index++) queue.removeLast();
for (let warm = 0; warm < 60; warm++) churn(queue, 200);
"#;

fn runtime(selection: JitSelection) -> Runtime {
    Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("array method leaf runtime")
}

fn run(runtime: &mut Runtime, source: &str) -> ExecutionResult {
    runtime
        .run_script(SourceInput::from_javascript(source), "array-leaves.js")
        .unwrap_or_else(|error| panic!("array method leaf fixture: {error:?}"))
}

fn completion(runtime: &mut Runtime, source: &str) -> String {
    run(runtime, source).completion_string().to_owned()
}

fn warmed(selection: JitSelection) -> Runtime {
    let mut runtime = runtime(selection);
    let result = run(&mut runtime, SETUP);
    if selection == JitSelection::ProductionTiered {
        let code_maps = result
            .jit_artifacts()
            .expect("leaf artifacts")
            .bundles()
            .iter()
            .filter(|bundle| bundle.manifest().tier() == JitDebugTier::Optimizing)
            .map(|bundle| {
                String::from_utf8(
                    bundle
                        .file(JitArtifactFileName::CodeMap)
                        .expect("leaf code map")
                        .contents()
                        .to_vec(),
                )
                .expect("UTF-8 code map")
            })
            .collect::<Vec<_>>();
        let leaf = |protected: bool| {
            code_maps.iter().any(|map| {
                map.contains("machineCacheIrLoadIntrinsicPrototype")
                    && map.contains("machineNativeLeafIdentity")
                    && map.contains("machineNativeLeafProbe")
                    && map.contains("machineCacheIrGuardArrayIndexProtector") == protected
            })
        };
        assert!(
            leaf(true),
            "push must reach a protected leaf: {code_maps:?}"
        );
        assert!(leaf(false), "pop/shift must reach a leaf: {code_maps:?}");
    }
    runtime
}

#[test]
fn warm_array_leaves_skip_the_general_method_boundary() {
    let mut runtime = warmed(JitSelection::ProductionTiered);
    let before = runtime.execution_stats();
    assert_eq!(completion(&mut runtime, "churn(queue, 1000)"), "1000000");
    let after = runtime.execution_stats();
    assert!(after.jit_optimized_entries > before.jit_optimized_entries);
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(after.jit_code_generations, before.jit_code_generations);
    assert_eq!(
        after.jit_to_rust_call_transitions - before.jit_to_rust_call_transitions,
        0,
        "every warm push/pop/shift must stay a leaf hit: before={before:?} after={after:?}"
    );
}

#[test]
fn misses_and_a_tripped_protector_match_the_interpreter() {
    const PROBES: &str = r#"
const results = [];
const full = new Queue();
for (let index = 0; index < 100; index++) full.add(index);
results.push(full.items.length, full.items[99]);
const holey = new Queue();
holey.items.length = 3;
results.push(holey.add(9), holey.items.length, 1 in holey.items, holey.items[3]);
const frozen = new Queue();
frozen.add(1);
Object.freeze(frozen.items);
try { frozen.add(2); results.push("no throw"); } catch (error) { results.push(error.name); }
try { frozen.removeLast(); results.push("no throw"); } catch (error) { results.push(error.name); }
let setterHits = 0;
Object.defineProperty(Array.prototype, "0", {
    get() { return "inherited"; },
    set(value) { setterHits += value; },
    configurable: true,
});
const fresh = new Queue();
results.push(fresh.add(5), fresh.items.length, setterHits, fresh.items[0],
    Object.prototype.hasOwnProperty.call(fresh.items, "0"));
delete Array.prototype[0];
results.push(churn(queue, 10));
JSON.stringify(results);
"#;
    let mut expected = None;
    for selection in TIERS {
        let mut runtime = warmed(selection);
        let actual = completion(&mut runtime, PROBES);
        match &expected {
            None => expected = Some(actual),
            Some(expected) => assert_eq!(&actual, expected, "{selection:?}"),
        }
    }
    assert_eq!(
        expected.as_deref(),
        Some(r#"[100,99,4,4,false,9,"TypeError","TypeError",1,1,5,"inherited",false,100]"#)
    );
}
