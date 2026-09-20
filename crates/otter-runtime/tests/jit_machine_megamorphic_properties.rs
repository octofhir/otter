//! Generated named loads through the isolate's shared property lookup cache.
//!
//! # Contents
//! - Terminal megamorphic sites reuse own and direct-prototype data entries.
//! - New shapes populate the same table without replacing compiled code.
//! - Live payloads, prototype changes, accessors and proxies retain semantics.
//! - Allocating cold getters preserve object identity under moving collection.
//!
//! # Invariants
//! - Successful probes load the current slot and never retain a cached Value.
//! - A failed proof enters the existing property boundary exactly once.
//! - Cache fills do not deoptimize, recompile or extend the per-site PIC.
//!
//! # See also
//! - `otter_vm::property_cache` owns the one isolate-wide lookup table.
//! - `jit_machine_generic_properties` covers immutable per-site programs.

use otter_runtime::{
    JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    RuntimeExecutionStats, SourceInput,
};

const SETUP: &str = r#"
const megaPrototype = { value: 66, marker: 6 };
const megaInherited = Object.create(megaPrototype);
megaInherited.inheritedMarker = 1;
const megaReceivers = [
    { value: 11, a: 1 },
    { b: 2, value: 22 },
    { c: 3, d: 4, value: 33 },
    { value: 44, e: 5, f: 6 },
    { g: 7, value: 55, h: 8 },
    megaInherited,
];
const megaArguments = [megaReceivers[0]];
function sharedMegamorphicLoad(receiver) {
    // A tiny single-load leaf has no predicted function-entry tiering payoff.
    // Keep a bounded scalar body while executing exactly one property read.
    for (let index = 0; index < 3; index++) {}
    return receiver.value;
}
for (let warm = 0; warm < 4010; warm++) {
    megaArguments[0] = megaReceivers[warm % megaReceivers.length];
    Reflect.apply(sharedMegamorphicLoad, undefined, megaArguments);
}
"#;

fn warmed() -> Runtime {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .expect("megamorphic property runtime");
    let result = runtime
        .run_script(SourceInput::from_javascript(SETUP), "megamorphic-setup.js")
        .expect("warm megamorphic named load");
    let bundle = result
        .jit_artifacts()
        .expect("captured megamorphic artifacts")
        .bundles()
        .iter()
        .find(|bundle| {
            bundle.manifest().function_name() == "sharedMegamorphicLoad"
                && bundle.manifest().tier() == JitDebugTier::Optimizing
        })
        .unwrap_or_else(|| panic!("missing Machine load: {:?}", result.jit_debug_report()));
    let regions = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::CodeMap)
            .expect("megamorphic code map")
            .contents(),
    )
    .expect("UTF-8 code map");
    assert!(
        regions.contains("machineMegamorphicPropertyLoad"),
        "{regions}"
    );
    assert_eq!(
        regions.matches("machinePropertyLoadCold").count(),
        1,
        "{regions}"
    );
    runtime
}

fn run(runtime: &mut Runtime, source: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), "megamorphic-probe.js")
        .unwrap_or_else(|error| panic!("megamorphic probe: {error:?}"))
        .completion_string()
        .to_owned()
}

fn assert_probe(before: RuntimeExecutionStats, after: RuntimeExecutionStats, property_misses: u64) {
    assert_completion(before, after, property_misses);
    assert_eq!(after.jit_compile_attempts, before.jit_compile_attempts);
    assert_eq!(after.jit_code_generations, before.jit_code_generations);
    assert_eq!(after.jit_feedback_refreshes, before.jit_feedback_refreshes);
}

fn assert_completion(
    before: RuntimeExecutionStats,
    after: RuntimeExecutionStats,
    property_misses: u64,
) {
    assert_no_replay(before, after);
    assert_eq!(
        after.jit_runtime_property_stubs - before.jit_runtime_property_stubs,
        property_misses,
    );
}

fn assert_no_replay(before: RuntimeExecutionStats, after: RuntimeExecutionStats) {
    assert!(after.jit_optimized_entries > before.jit_optimized_entries);
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(
        after.jit_generated_call_deopts,
        before.jit_generated_call_deopts
    );
}

#[test]
fn megamorphic_own_and_prototype_hits_remain_generated() {
    let mut runtime = warmed();
    let before = runtime.execution_stats();
    assert_eq!(
        run(
            &mut runtime,
            r#"
const results = [];
for (let index = 0; index < megaReceivers.length; index++) {
    megaArguments[0] = megaReceivers[index];
    results.push(Reflect.apply(sharedMegamorphicLoad, undefined, megaArguments));
}
JSON.stringify(results);
"#
        ),
        "[11,22,33,44,55,66]",
    );
    assert_probe(before, runtime.execution_stats(), 0);
}

#[test]
fn a_new_shape_fills_the_shared_cache_and_reads_live_payloads() {
    let mut runtime = warmed();
    run(
        &mut runtime,
        "const freshMega = { novel: 1, value: 73, tail: 2 }; megaArguments[0] = freshMega;",
    );
    let before = runtime.execution_stats();
    assert_eq!(
        run(
            &mut runtime,
            "Reflect.apply(sharedMegamorphicLoad, undefined, megaArguments);"
        ),
        "73"
    );
    assert_probe(before, runtime.execution_stats(), 1);
    run(
        &mut runtime,
        "freshMega.value = 'current'; freshMega[Symbol('metadata')] = 1;",
    );
    let before = runtime.execution_stats();
    assert_eq!(
        run(
            &mut runtime,
            "Reflect.apply(sharedMegamorphicLoad, undefined, megaArguments);"
        ),
        "current"
    );
    assert_probe(before, runtime.execution_stats(), 0);
}

#[test]
fn current_prototype_and_descriptor_remain_authoritative() {
    let mut runtime = warmed();
    run(
        &mut runtime,
        r#"
const replacementPrototype = { value: 77, marker: 7 };
Object.setPrototypeOf(megaInherited, replacementPrototype);
megaArguments[0] = megaInherited;
"#,
    );
    let before = runtime.execution_stats();
    assert_eq!(
        run(
            &mut runtime,
            "Reflect.apply(sharedMegamorphicLoad, undefined, megaArguments);"
        ),
        "77"
    );
    // An equal holder shape authorizes a fresh load from the current prototype.
    assert_probe(before, runtime.execution_stats(), 0);
    run(
        &mut runtime,
        r#"
let megaGetterCalls = 0;
Object.defineProperty(replacementPrototype, 'value', { configurable: true, get() {
    megaGetterCalls++;
    return 88;
} });
"#,
    );
    let before = runtime.execution_stats();
    assert_eq!(
        run(
            &mut runtime,
            "JSON.stringify([Reflect.apply(sharedMegamorphicLoad, undefined, megaArguments), megaGetterCalls]);"
        ),
        "[88,1]"
    );
    assert_probe(before, runtime.execution_stats(), 1);
}

#[test]
fn allocating_getter_and_proxy_misses_commit_once_with_moving_roots() {
    let mut runtime = warmed();
    run(
        &mut runtime,
        r#"
let megaEffects = 0;
const megaSentinel = { preserved: 91 };
const megaGetter = { get value() {
    megaEffects++;
    const retained = [];
    for (let index = 0; index < 128; index++) retained.push({ index });
    if (retained[127].index !== 127) throw new Error('lost allocation roots');
    return megaSentinel;
} };
megaArguments[0] = megaGetter;
"#,
    );
    let before = runtime.execution_stats();
    let getter_probe = runtime.run_script(
        SourceInput::from_javascript("JSON.stringify([Reflect.apply(sharedMegamorphicLoad, undefined, megaArguments) === megaSentinel, megaEffects]);"),
        "megamorphic-allocating-getter.js",
    ).expect("allocating getter probe");
    assert_eq!(getter_probe.completion_string(), "[true,1]");
    let after = runtime.execution_stats();
    assert_no_replay(before, after);
    // Runtime totals also include the getter's own property operations. The
    // one caller cold region and megaEffects === 1 above prove exactly-once
    // completion; nested transitions must not be attributed to that caller.
    assert!(after.jit_runtime_property_stubs > before.jit_runtime_property_stubs);
    // The getter's allocation loop may compile on its first invocation. The
    // property caller must retain its existing generation through that reentry.
    let bundles = getter_probe
        .jit_artifacts()
        .expect("getter probe artifacts")
        .bundles();
    assert!(
        bundles
            .iter()
            .all(|bundle| bundle.manifest().function_name() != "sharedMegamorphicLoad")
    );
    if let Some(stride @ (1 | 4 | 16)) = std::env::var("OTTER_GC_STRESS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        assert!(after.gc_minor_cycles - before.gc_minor_cycles >= 128 / stride);
        assert!(after.gc_minor_slot_updates > before.gc_minor_slot_updates);
    }
    run(
        &mut runtime,
        r#"
megaArguments[0] = new Proxy({ value: 92 }, { get(target, key) {
    megaEffects++;
    return target[key];
} });
"#,
    );
    let before = runtime.execution_stats();
    assert_eq!(
        run(
            &mut runtime,
            "JSON.stringify([Reflect.apply(sharedMegamorphicLoad, undefined, megaArguments), megaEffects]);"
        ),
        "[92,2]"
    );
    assert_probe(before, runtime.execution_stats(), 1);
}
