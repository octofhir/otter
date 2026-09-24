//! Generated property loads from Map and Set prototypes.
//!
//! # Contents
//! - Warm named lookups prove generated hits without native-call traffic.
//! - Live prototype slots, own shadows, overrides and accessors retain semantics.
//! - Explicit calls preserve the loaded callable across argument evaluation.
//! - Moving collection preserves String, Map, receiver and callable roots.
//! - Primitive String lookups retain the canonical property semantics.
//! - The unmodified native-boundary kernel prepares fresh realm prototypes.
//!
//! # Invariants
//! - A failed receiver or holder proof commits one canonical property lookup.
//! - Prototype mutation cannot resurrect a cached callable or replay a getter.
//! - Generated hits neither allocate nor publish a runtime property transition.
//!
//! # See also
//! - `jit_machine_generic_properties` covers ordinary receiver property programs.
//! - `jit_machine_native_call_with_this` covers generated Math call bodies.

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
const builtinString = 'otter';
const builtinMap = new Map([['key', 73]]);
const builtinSet = new Set(['key']);
const originalCharCodeAt = String.prototype.charCodeAt;
const originalMapGet = Map.prototype.get;
const originalMapSet = Map.prototype.set;
const originalSetHas = Set.prototype.has;
const originalSetAdd = Set.prototype.add;
function builtinStringLoad(receiver) {
    for (let index = 0; index < 3; index++) {}
    return receiver.charCodeAt;
}
function builtinMapGetLoad(receiver) {
    for (let index = 0; index < 3; index++) {}
    return receiver.get;
}
function builtinMapSetLoad(receiver) {
    for (let index = 0; index < 3; index++) {}
    return receiver.set;
}
function builtinSetHasLoad(receiver) {
    for (let index = 0; index < 3; index++) {}
    return receiver.has;
}
function builtinSetAddLoad(receiver) {
    for (let index = 0; index < 3; index++) {}
    return receiver.add;
}
const builtinStringArguments = [builtinString];
const builtinMapArguments = [builtinMap];
const builtinSetArguments = [builtinSet];
for (let warm = 0; warm < 4010; warm++) {
    Reflect.apply(builtinStringLoad, undefined, builtinStringArguments);
    Reflect.apply(builtinMapGetLoad, undefined, builtinMapArguments);
    Reflect.apply(builtinMapSetLoad, undefined, builtinMapArguments);
    Reflect.apply(builtinSetHasLoad, undefined, builtinSetArguments);
    Reflect.apply(builtinSetAddLoad, undefined, builtinSetArguments);
}
"#;

fn runtime(selection: JitSelection) -> Runtime {
    Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .expect("builtin property runtime")
}

fn run(runtime: &mut Runtime, source: &str) -> ExecutionResult {
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "builtin-properties.js",
        )
        .unwrap_or_else(|error| panic!("builtin property fixture: {error:?}"))
}

fn completion(runtime: &mut Runtime, source: &str) -> String {
    run(runtime, source).completion_string().to_owned()
}

fn assert_generated_artifact(result: &ExecutionResult, name: &str) {
    let bundles: Vec<_> = result
        .jit_artifacts()
        .expect("builtin property artifacts")
        .bundles()
        .iter()
        .filter(|bundle| {
            bundle.manifest().function_name() == name
                && bundle.manifest().tier() == JitDebugTier::Optimizing
        })
        .collect();
    assert_eq!(
        bundles.len(),
        1,
        "{name} must have one optimizing body: {:?}",
        result.jit_debug_report()
    );
    let code_map = std::str::from_utf8(
        bundles[0]
            .file(JitArtifactFileName::CodeMap)
            .expect("builtin property code map")
            .contents(),
    )
    .expect("UTF-8 code map");
    assert!(
        code_map.contains("machineCacheIrLoadField"),
        "{name}: {code_map}"
    );
    assert!(
        code_map.contains("machineCacheIrLoadIntrinsicPrototype"),
        "{name}: {code_map}"
    );
    assert_eq!(
        code_map.matches("machinePropertyLoadCold").count(),
        1,
        "{name}: {code_map}"
    );
}

fn warmed(selection: JitSelection) -> Runtime {
    let mut runtime = runtime(selection);
    let result = run(&mut runtime, SETUP);
    if selection != JitSelection::InterpreterOnly {
        for name in [
            "builtinMapGetLoad",
            "builtinMapSetLoad",
            "builtinSetHasLoad",
            "builtinSetAddLoad",
        ] {
            if selection == JitSelection::ProductionTiered {
                assert_generated_artifact(&result, name);
            } else {
                assert!(
                    result
                        .jit_artifacts()
                        .expect("template property artifacts")
                        .bundles()
                        .iter()
                        .any(|bundle| bundle.manifest().function_name() == name
                            && bundle.manifest().tier() == JitDebugTier::Template),
                    "{name} must have a Template body: {:?}",
                    result.jit_debug_report()
                );
            }
        }
    }
    runtime
}

fn assert_probe(
    before: RuntimeExecutionStats,
    after: RuntimeExecutionStats,
    misses: u64,
    selection: JitSelection,
) {
    if selection == JitSelection::ProductionTiered {
        assert!(after.jit_optimized_entries > before.jit_optimized_entries);
    }
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(
        after.jit_generated_call_deopts,
        before.jit_generated_call_deopts
    );
    assert_eq!(after.jit_code_generations, before.jit_code_generations);
    assert_eq!(after.jit_feedback_refreshes, before.jit_feedback_refreshes);
    assert_eq!(
        after.jit_runtime_property_stubs - before.jit_runtime_property_stubs,
        misses,
        "isolated lookup must use the expected number of cold property transitions"
    );
}

fn assert_stress_relocation(before: RuntimeExecutionStats, after: RuntimeExecutionStats) {
    if let Some(stride @ (1 | 4 | 16)) = std::env::var("OTTER_GC_STRESS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        assert!(after.gc_minor_cycles - before.gc_minor_cycles >= 128 / stride);
        assert!(after.gc_minor_slot_updates > before.gc_minor_slot_updates);
    }
}

#[test]
fn builtin_prototype_lookups_stay_generated_and_read_live_slots() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = warmed(selection);
        let before = runtime.execution_stats();
        assert_eq!(
            completion(
                &mut runtime,
                r#"
JSON.stringify([
    Reflect.apply(builtinMapGetLoad, undefined, builtinMapArguments) === originalMapGet,
    Reflect.apply(builtinMapSetLoad, undefined, builtinMapArguments) === originalMapSet,
    Reflect.apply(builtinSetHasLoad, undefined, builtinSetArguments) === originalSetHas,
    Reflect.apply(builtinSetAddLoad, undefined, builtinSetArguments) === originalSetAdd,
]);
"#,
            ),
            "[true,true,true,true]"
        );
        assert_probe(before, runtime.execution_stats(), 0, selection);

        run(
            &mut runtime,
            r#"
function replacementGet() { return 82; }
function replacementSet() { return 83; }
function replacementHas() { return 84; }
function replacementAdd() { return 85; }
Map.prototype.get = replacementGet;
Map.prototype.set = replacementSet;
Set.prototype.has = replacementHas;
Set.prototype.add = replacementAdd;
Map.prototype[Symbol('map metadata')] = 2;
Set.prototype[Symbol('set metadata')] = 3;
"#,
        );
        let before = runtime.execution_stats();
        assert_eq!(
            completion(
                &mut runtime,
                r#"
JSON.stringify([
    Reflect.apply(builtinMapGetLoad, undefined, builtinMapArguments) === replacementGet,
    Reflect.apply(builtinMapSetLoad, undefined, builtinMapArguments) === replacementSet,
    Reflect.apply(builtinSetHasLoad, undefined, builtinSetArguments) === replacementHas,
    Reflect.apply(builtinSetAddLoad, undefined, builtinSetArguments) === replacementAdd,
]);
"#,
            ),
            "[true,true,true,true]"
        );
        assert_probe(before, runtime.execution_stats(), 0, selection);
    }
}

#[test]
fn native_boundary_kernel_prepares_collection_prototypes_without_javascript_reads() {
    const KERNEL: &str = include_str!("../../../benchmarks/scripts/native-boundary.js");
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = runtime(selection);
        // No fixture reads Map.prototype: its initial dictionary must be
        // prepared by compilation before the immutable proof is captured.
        run(&mut runtime, KERNEL);
        let first = run(&mut runtime, "engineKernel();");
        assert_eq!(first.completion_string(), "27000000", "{selection:?}");
        if selection == JitSelection::ProductionTiered {
            let bundle = first
                .jit_artifacts()
                .expect("kernel artifacts")
                .bundles()
                .iter()
                .find(|bundle| {
                    bundle.manifest().function_name() == "engineKernel"
                        && bundle.manifest().tier() == JitDebugTier::Optimizing
                })
                .unwrap_or_else(|| panic!("kernel must optimize: {:?}", first.jit_debug_report()));
            let code_map = std::str::from_utf8(
                bundle
                    .file(JitArtifactFileName::CodeMap)
                    .expect("kernel code map")
                    .contents(),
            )
            .expect("UTF-8 kernel code map");
            // Map get/set and String charCodeAt property loads plus the
            // String `indexOf` method proof; both String holders are pinned by
            // their dictionary layout id. `indexOf` and the already-loaded
            // `charCodeAt`, Map `get` and Map `set` callees complete in
            // declared leaves.
            for (region, count) in [
                ("machineCacheIrLoadIntrinsicPrototype", 4),
                ("machineCacheIrGuardDictionaryLayout", 2),
                ("machineNativeLeafProbe", 4),
            ] {
                assert_eq!(
                    code_map.matches(region).count(),
                    count,
                    "the original kernel must generate {region}: {code_map}"
                );
            }
        }
        let before = runtime.execution_stats();
        assert_eq!(completion(&mut runtime, "engineKernel();"), "27000000");
        let after = runtime.execution_stats();
        // x86 Template retains its existing indexed load, String.length and
        // Math slot cold paths. Its original kernel used 1,600,000 property
        // transitions; generated Map get/set and String charCodeAt loads
        // remove exactly 600,000.
        let expected_property_stubs =
            if cfg!(target_arch = "x86_64") && selection == JitSelection::Template {
                1_000_000
            } else {
                0
            };
        assert_eq!(
            after.jit_runtime_property_stubs - before.jit_runtime_property_stubs,
            expected_property_stubs,
            "{selection:?}: only the x86 Template cold property paths may remain"
        );
        if selection == JitSelection::ProductionTiered {
            // A fresh Map takes 64 inserting `set` calls, which may allocate,
            // plus its construct; every other call completes in a leaf hit.
            assert_eq!(
                after.jit_to_rust_call_transitions - before.jit_to_rust_call_transitions,
                65,
                "only Map insertions and the Map construct may cross"
            );
        }
        assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
        assert_eq!(
            after.jit_generated_call_deopts,
            before.jit_generated_call_deopts
        );
    }
}

#[test]
fn builtin_prototype_accessor_misses_execute_once() {
    for selection in TIERS {
        let mut runtime = warmed(selection);
        run(
            &mut runtime,
            r#"
let stringGets = 0;
let mapGets = 0;
let mapSets = 0;
let setHas = 0;
let setAdds = 0;
Object.defineProperty(String.prototype, 'charCodeAt', { configurable: true, get() {
    stringGets++; return originalCharCodeAt;
} });
Object.defineProperty(Map.prototype, 'get', { configurable: true, get() {
    mapGets++; return originalMapGet;
} });
Object.defineProperty(Map.prototype, 'set', { configurable: true, get() {
    mapSets++; return originalMapSet;
} });
Object.defineProperty(Set.prototype, 'has', { configurable: true, get() {
    setHas++; return originalSetHas;
} });
Object.defineProperty(Set.prototype, 'add', { configurable: true, get() {
    setAdds++; return originalSetAdd;
} });
"#,
        );
        let before = runtime.execution_stats();
        assert_eq!(
            completion(
                &mut runtime,
                r#"
JSON.stringify([
    Reflect.apply(builtinStringLoad, undefined, builtinStringArguments) === originalCharCodeAt,
    Reflect.apply(builtinMapGetLoad, undefined, builtinMapArguments) === originalMapGet,
    Reflect.apply(builtinMapSetLoad, undefined, builtinMapArguments) === originalMapSet,
    Reflect.apply(builtinSetHasLoad, undefined, builtinSetArguments) === originalSetHas,
    Reflect.apply(builtinSetAddLoad, undefined, builtinSetArguments) === originalSetAdd,
    stringGets, mapGets, mapSets, setHas, setAdds,
]);
"#,
            ),
            "[true,true,true,true,true,1,1,1,1,1]",
            "{selection:?}"
        );
        if selection == JitSelection::ProductionTiered {
            assert_probe(before, runtime.execution_stats(), 5, selection);
        }
    }
}

#[test]
fn own_shadows_and_custom_prototypes_keep_canonical_precedence() {
    for selection in TIERS {
        let mut runtime = warmed(selection);
        assert_eq!(
            completion(
                &mut runtime,
                r#"
const results = [];
results.push(builtinMapGetLoad(builtinSet) === undefined, builtinSetAddLoad(builtinMap) === undefined);
const deletionMap = new Map();
const deletionSet = new Set();
let inheritedGets = 0;
Object.defineProperty(Object.prototype, 'get', { configurable: true, get() {
    inheritedGets++; return 101;
} });
Object.defineProperty(Object.prototype, 'add', { configurable: true, get() {
    inheritedGets++; return 102;
} });
delete Map.prototype.get;
delete Set.prototype.add;
results.push(builtinMapGetLoad(deletionMap), builtinSetAddLoad(deletionSet), inheritedGets);
delete Object.prototype.get;
delete Object.prototype.add;
Object.defineProperty(Map.prototype, 'get', { configurable: true, writable: true, value: originalMapGet });
Object.defineProperty(Set.prototype, 'add', { configurable: true, writable: true, value: originalSetAdd });
builtinMap.get = 91;
results.push(builtinMapGetLoad(builtinMap));
delete builtinMap.get;
results.push(builtinMapGetLoad(builtinMap) === originalMapGet);
const customMapPrototype = { get: 92, set: 93 };
Object.setPrototypeOf(builtinMap, customMapPrototype);
results.push(builtinMapGetLoad(builtinMap), builtinMapSetLoad(builtinMap));
let gets = 0;
Object.defineProperty(customMapPrototype, 'get', { get() { gets++; return 94; } });
results.push(builtinMapGetLoad(builtinMap), gets);
Object.setPrototypeOf(builtinMap, null);
results.push(builtinMapGetLoad(builtinMap) === undefined);
const boxed = new String('boxed');
boxed.charCodeAt = 95;
results.push(builtinStringLoad(boxed));
const sentinel = {};
Object.defineProperty(String.prototype, 'charCodeAt', { get() { gets++; throw sentinel; } });
try { builtinStringLoad('primitive'); } catch (error) { results.push(error === sentinel, gets); }
builtinSet.add = 96;
results.push(builtinSetAddLoad(builtinSet));
delete builtinSet.add;
results.push(builtinSetAddLoad(builtinSet) === originalSetAdd);
Object.setPrototypeOf(builtinSet, { has: 97, add: 98 });
results.push(builtinSetHasLoad(builtinSet), builtinSetAddLoad(builtinSet));
JSON.stringify(results);
"#,
            ),
            "[true,true,101,102,2,91,true,92,93,94,1,true,95,true,2,96,true,97,98]",
            "{selection:?}"
        );
    }
}

const CALL_SETUP: &str = r#"
const argumentMap = new Map([['key', 73]]);
let allocatingArgument = false;
let argumentString = 'otter';
let argumentEffects = 0;
function propertyArgument() {
    if (allocatingArgument) {
        argumentEffects++;
        Map.prototype.get = function() { return 99; };
        String.prototype.charCodeAt = function() { return 99; };
        const retained = [];
        for (let index = 0; index < 128; index++) retained.push({ index });
        if (retained[127].index !== 127) throw new Error('argument roots lost');
    }
    return 0;
}
function callStringAfterLookup(receiver) { return receiver.charCodeAt(propertyArgument() | 0); }
function callMapAfterLookup(receiver) { return receiver.get(propertyArgument() ? 'other' : 'key'); }
for (let warm = 0; warm < 4010; warm++) {
    callStringAfterLookup(argumentString);
    callMapAfterLookup(argumentMap);
}
const originalArgumentCharCodeAt = String.prototype.charCodeAt;
const originalArgumentGet = Map.prototype.get;
allocatingArgument = true;
"#;

#[test]
fn loaded_builtin_callables_survive_prototype_mutation_and_argument_collection() {
    for selection in TIERS {
        let mut runtime = runtime(selection);
        let setup = run(&mut runtime, CALL_SETUP);
        if selection == JitSelection::ProductionTiered {
            assert_generated_artifact(&setup, "callMapAfterLookup");
        }
        let before = runtime.execution_stats();
        assert_eq!(
            completion(
                &mut runtime,
                r#"
argumentString = ['o', 't', 't', 'e', 'r'].join('');
const stringResult = callStringAfterLookup(argumentString);
Map.prototype.get = originalArgumentGet;
const youngArgumentMap = new Map([['key', 73]]);
const mapResult = callMapAfterLookup(youngArgumentMap);
const callableToken = {};
Map.prototype.get = (function(token) {
    return function(key) { return [this === youngArgumentMap, key, token]; };
})(callableToken);
// Argument evaluation drops the only property edge to this young closure.
const closureResult = callMapAfterLookup(youngArgumentMap);
JSON.stringify([
    stringResult, mapResult, argumentEffects,
    closureResult[0], closureResult[1], closureResult[2] === callableToken,
]);
"#,
            ),
            r#"[111,73,3,true,"key",true]"#,
            "{selection:?}"
        );
        assert_stress_relocation(before, runtime.execution_stats());
    }
}

#[test]
fn allocating_property_getters_keep_moving_receivers_and_results_alive() {
    for selection in TIERS {
        let mut runtime = warmed(selection);
        run(
            &mut runtime,
            r#"
let allocatingGets = 0;
let getterMapReceiver;
function allocatingMethodGetter() {
    allocatingGets++;
    const receiver = this;
    const result = function() { return receiver; };
    const retained = [];
    for (let index = 0; index < 128; index++) retained.push({ index });
    if (retained[127].index !== 127) throw new Error('getter roots lost');
    return result;
}
Object.defineProperty(Map.prototype, 'get', { configurable: true, get: allocatingMethodGetter });
Object.defineProperty(String.prototype, 'charCodeAt', { configurable: true, get: allocatingMethodGetter });
"#,
        );
        let before = runtime.execution_stats();
        assert_eq!(
            completion(
                &mut runtime,
                r#"
getterMapReceiver = new Map();
const stringReceiver = ['m', 'o', 'v', 'i', 'n', 'g'].join('');
const mapMethod = builtinMapGetLoad(getterMapReceiver);
const stringMethod = builtinStringLoad(stringReceiver);
JSON.stringify([mapMethod() === getterMapReceiver, String(stringMethod()) === stringReceiver, allocatingGets]);
"#,
            ),
            "[true,true,2]",
            "{selection:?}"
        );
        assert_stress_relocation(before, runtime.execution_stats());
    }
}
