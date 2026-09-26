//! Generated `%Function.prototype%` loads on ordinary closure receivers.
//!
//! # Contents
//! - Warm `f.apply` / `f.call` loads on bag-free ordinary closures complete
//!   through the intrinsic-prototype program without property transitions,
//!   and reading a closure property never materializes an own bag.
//! - An own shadowing property, a `[[Prototype]]` override, a generator kind,
//!   and a replaced or deleted `%Function.prototype%` method all match the
//!   interpreter in every tier.
//!
//! # Invariants
//! - A proof miss commits one canonical property load; no tier deopts.
//!
//! # See also
//! - `jit_machine_builtin_properties` covers the collection and string
//!   intrinsic-prototype programs sharing the same Machine op.

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
function plain(value) { return value + 1; }
function readApply(f, count, expected = Function.prototype.apply) {
    let hits = 0;
    for (let index = 0; index < count; index++) {
        if (f.apply === expected) hits++;
    }
    return hits;
}
function readCall(f) { return f.call; }
function forward() { return plain.apply(this, arguments); }
for (let warm = 0; warm < 60; warm++) {
    readApply(plain, 200);
    readCall(plain);
    forward(warm);
}
"#;

fn runtime(selection: JitSelection) -> Runtime {
    Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("closure property runtime")
}

fn run(runtime: &mut Runtime, source: &str) -> ExecutionResult {
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "closure-properties.js",
        )
        .unwrap_or_else(|error| panic!("closure property fixture: {error:?}"))
}

fn completion(runtime: &mut Runtime, source: &str) -> String {
    run(runtime, source).completion_string().to_owned()
}

fn warmed(selection: JitSelection) -> Runtime {
    let mut runtime = runtime(selection);
    let result = run(&mut runtime, SETUP);
    if selection == JitSelection::ProductionTiered {
        let bundle = result
            .jit_artifacts()
            .expect("closure artifacts")
            .bundles()
            .iter()
            .find(|bundle| {
                bundle.manifest().function_name() == "readApply"
                    && bundle.manifest().tier() == JitDebugTier::Optimizing
            })
            .unwrap_or_else(|| panic!("readApply must optimize: {:?}", result.jit_debug_report()));
        let code_map = std::str::from_utf8(
            bundle
                .file(JitArtifactFileName::CodeMap)
                .expect("closure code map")
                .contents(),
        )
        .expect("UTF-8 code map");
        assert!(
            code_map.contains("machineCacheIrLoadIntrinsicPrototype"),
            "readApply lacks the closure proof: {code_map}"
        );
    }
    runtime
}

#[test]
fn warm_closure_prototype_loads_skip_the_property_boundary() {
    let mut runtime = warmed(JitSelection::ProductionTiered);
    let before = runtime.execution_stats();
    assert_eq!(completion(&mut runtime, "readApply(plain, 5000)"), "5000");
    let after = runtime.execution_stats();
    assert!(after.jit_optimized_entries > before.jit_optimized_entries);
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert!(
        after.jit_runtime_property_stubs - before.jit_runtime_property_stubs < 16,
        "warm f.apply loads must stay generated: {} stubs",
        after.jit_runtime_property_stubs - before.jit_runtime_property_stubs
    );
    assert_eq!(
        completion(
            &mut runtime,
            "const fresh = function () {}; fresh.missing; Object.getOwnPropertyNames(fresh).join()"
        ),
        "length,name,prototype"
    );
}

#[test]
fn closure_lookup_changes_match_the_interpreter() {
    const PROBES: &str = r#"
const results = [];
function shadowed() {}
shadowed.apply = "own";
results.push(readApply(shadowed, 3), String(readCall(shadowed) === Function.prototype.call));
results.push(typeof readCall(shadowed), shadowed.apply);
function reparented() {}
Object.setPrototypeOf(reparented, { apply: "inherited", call: "c" });
results.push(readApply(reparented, 3), readCall(reparented));
Object.setPrototypeOf(reparented, Function.prototype);
results.push(readApply(reparented, 3));
function* generator() {}
Object.getPrototypeOf(generator).call = "generator call";
results.push(readCall(generator), readCall(plain) === Function.prototype.call);
delete Object.getPrototypeOf(generator).call;
const originalCall = Function.prototype.call;
Function.prototype.call = "replaced";
results.push(readCall(plain), readCall(generator));
Function.prototype.call = originalCall;
const originalApply = Function.prototype.apply;
delete Function.prototype.apply;
results.push(readApply(plain, 3), typeof plain.apply);
Function.prototype.apply = originalApply;
results.push(readApply(plain, 3), forward(41));
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
        Some(
            r#"[0,"true","function","own",0,"c",3,"generator call",true,"replaced","replaced",3,"undefined",3,42]"#
        )
    );
}
