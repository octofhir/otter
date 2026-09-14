//! Machine IR bounded method-chain and cold-call-exit coverage.
//!
//! # Contents
//! - Four guarded method candidates at one source site, including two receiver
//!   shapes that share a function id but require different `this` values.
//! - Already-started candidate overflow/throw completion without call replay.
//! - A fifth receiver shape that completes generically while retaining the caller
//!   generation without duplicating the method effect.
//! - Never-taken plain and method calls represented as exact cold exits, plus a
//!   warmed bootstrap `parseInt(Int32)` control that remains a generated leaf.
//! - Moving-GC coverage for live receivers and arguments through method-chain
//!   candidates two through four.
//!
//! # Invariants
//! - A complete four-target snapshot emits one dense guard/call chain and does
//!   not deopt its outer Machine activation on a known receiver.
//! - Candidate misses and cold calls leave the source operation uncommitted;
//!   the canonical interpreter observes every user effect exactly once.
//! - Each generated direct-call relocation carries its candidate index and the
//!   complete target count, so artifacts prove the whole chain was emitted.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTier,
    JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";

const METHOD_CHAIN_SETUP: &str = r#"
globalThis.__machineChainFifthEffects = 0;

function machineChainShared(value) {
  return this.base + value;
}

function machineChainAbrupt(value) {
  if (this.kind === "overflow") {
    this.effects++;
    return value + 1;
  }
  this.effects++;
  if (value < 0) throw "chain-boom";
  return this.base + value;
}

function machineChainFifth(value) {
  globalThis.__machineChainFifthEffects++;
  return this.base + value;
}

globalThis.__machineChainReceiver0 = { base: 100, method: machineChainShared };
globalThis.__machineChainReceiver1 = {
  kind: "overflow",
  base: 200,
  effects: 0,
  method: machineChainAbrupt
};
globalThis.__machineChainReceiver2 = {
  kind: "throw",
  marker: true,
  base: 300,
  effects: 0,
  method: machineChainAbrupt
};
globalThis.__machineChainReceiver3 = {
  kind: "shared",
  marker: true,
  padding: 0,
  base: 400,
  method: machineChainShared
};
globalThis.__machineChainReceiver4 = {
  fifth: true,
  a: 1,
  b: 2,
  c: 3,
  base: 500,
  method: machineChainFifth
};
globalThis.__machineChainReceivers = [
  __machineChainReceiver0,
  __machineChainReceiver1,
  __machineChainReceiver2,
  __machineChainReceiver3
];

function machineMethodChain(receiver, value) {
  return receiver.method(value);
}

for (let warm = 0; warm < 5000; warm++) {
  machineMethodChain(__machineChainReceivers[warm & 3], warm & 1023);
}
globalThis.__machineChainReceiver1.effects = 0;
globalThis.__machineChainReceiver2.effects = 0;
"#;

const METHOD_CHAIN_SAFE: &str = r#"
JSON.stringify([
  machineMethodChain(__machineChainReceiver0, 1),
  machineMethodChain(__machineChainReceiver1, 2),
  machineMethodChain(__machineChainReceiver2, 3),
  machineMethodChain(__machineChainReceiver3, 4),
  machineMethodChain(__machineChainReceiver0, 7),
  machineMethodChain(__machineChainReceiver3, 7)
]);
"#;

const METHOD_CHAIN_ABRUPT: &str = r#"
(() => {
const overflowBefore = __machineChainReceiver1.effects;
const overflow = machineMethodChain(__machineChainReceiver1, 2147483647);
const overflowEffects = __machineChainReceiver1.effects - overflowBefore;

const throwBefore = __machineChainReceiver2.effects;
let caught = "missing";
try {
  machineMethodChain(__machineChainReceiver2, -1);
} catch (error) {
  caught = error;
}
const throwEffects = __machineChainReceiver2.effects - throwBefore;
const recovered = machineMethodChain(__machineChainReceiver0, 42);
return JSON.stringify([overflow, overflowEffects, caught, throwEffects, recovered]);
})();
"#;

const METHOD_CHAIN_FIFTH_ONCE: &str = r#"
(() => {
const effectsBefore = __machineChainFifthEffects;
const result = machineMethodChain(__machineChainReceiver4, 5);
return JSON.stringify([result, __machineChainFifthEffects - effectsBefore]);
})();
"#;

const METHOD_CHAIN_FIFTH_RECOMPILE: &str = r#"
(() => {
const effectsBefore = __machineChainFifthEffects;
let checksum = 0;
for (let i = 0; i < 256; i++) {
  checksum += machineMethodChain(__machineChainReceiver4, i & 7);
}
return JSON.stringify([
  checksum,
  __machineChainFifthEffects - effectsBefore,
  machineMethodChain(__machineChainReceiver0, 9)
]);
})();
"#;

const COLD_CALL_SETUP: &str = r#"
globalThis.__machineColdPlainEffects = 0;
globalThis.__machineColdMethodEffects = 0;

function machineColdPlainTarget(value) {
  globalThis.__machineColdPlainEffects++;
  return value + 10;
}

function machineColdMethodTarget(value) {
  globalThis.__machineColdMethodEffects++;
  return this.bias + value;
}

globalThis.__machineColdMethodReceiver = {
  bias: 7,
  run: machineColdMethodTarget
};

function machineColdPlain(takeCold, fn, value) {
  if (takeCold) return fn(value);
  return value + 1;
}

function machineColdMethod(takeCold, receiver, value) {
  if (takeCold) return receiver.run(value);
  return value + 2;
}

function parseIntColdExitControl(value) {
  return parseInt(value);
}

for (let warm = 0; warm < 5000; warm++) {
  const value = (warm & 1023) - 512;
  machineColdPlain(false, machineColdPlainTarget, value);
  machineColdMethod(false, __machineColdMethodReceiver, value);
  parseIntColdExitControl(value);
}
"#;

const COLD_CALL_HOT_PROBE: &str = r#"
JSON.stringify([
  machineColdPlain(false, machineColdPlainTarget, 41),
  machineColdMethod(false, __machineColdMethodReceiver, 41),
  parseIntColdExitControl(41)
]);
"#;

const COLD_PLAIN_PROBE: &str = r#"
(() => {
const effectsBefore = __machineColdPlainEffects;
const result = machineColdPlain(true, machineColdPlainTarget, 41);
return JSON.stringify([result, __machineColdPlainEffects - effectsBefore]);
})();
"#;

const COLD_METHOD_PROBE: &str = r#"
(() => {
const effectsBefore = __machineColdMethodEffects;
const result = machineColdMethod(true, __machineColdMethodReceiver, 41);
return JSON.stringify([result, __machineColdMethodEffects - effectsBefore]);
})();
"#;

const METHOD_CHAIN_GC_SETUP: &str = r#"
globalThis.__machineChainGcSink = null;

function machineChainGcMethod(argument, allocationCount) {
  for (let i = 0; i < allocationCount; i++) {
    globalThis.__machineChainGcSink = { index: i, padding: i + 1 };
  }
  return this.base + argument.value;
}

globalThis.__machineChainGcReceiver0 = { base: 100, method: machineChainGcMethod };
globalThis.__machineChainGcReceiver1 = {
  one: 1,
  base: 200,
  method: machineChainGcMethod
};
globalThis.__machineChainGcReceiver2 = {
  one: 1,
  two: 2,
  base: 300,
  method: machineChainGcMethod
};
globalThis.__machineChainGcReceiver3 = {
  one: 1,
  two: 2,
  three: 3,
  base: 400,
  method: machineChainGcMethod
};
globalThis.__machineChainGcReceivers = [
  __machineChainGcReceiver0,
  __machineChainGcReceiver1,
  __machineChainGcReceiver2,
  __machineChainGcReceiver3
];
globalThis.__machineChainGcWarmArgument = { value: 0 };

function machineMethodChainGc(receiver, argument, allocationCount) {
  return receiver.method(argument, allocationCount);
}

for (let warm = 0; warm < 5000; warm++) {
  machineMethodChainGc(
    __machineChainGcReceivers[warm & 3],
    __machineChainGcWarmArgument,
    0
  );
}
"#;

const COMPILED_GENERIC_METHOD_ATTEMPT: &str = r#"
globalThis.__compiledGenericMethodEffects = 0;
globalThis.__compiledGenericReceiver = {
  run(value) {
    globalThis.__compiledGenericMethodEffects++;
    return value + 10;
  }
};

function compiledGenericMethodAttempt(takeCold, receiver, value) {
  if (takeCold) return receiver.run(value);
  return value + 1;
}

for (let warm = 0; warm < 64; warm++) {
  compiledGenericMethodAttempt(false, __compiledGenericReceiver, warm);
}
const attempted = compiledGenericMethodAttempt(
  true,
  __compiledGenericReceiver,
  32
);
for (let warm = 0; warm < 5000; warm++) {
  compiledGenericMethodAttempt(false, __compiledGenericReceiver, warm);
}
JSON.stringify([attempted, __compiledGenericMethodEffects]);
"#;

#[derive(Clone, Copy, Debug)]
struct CounterDelta {
    optimized_entries: u64,
    optimized_deopts: u64,
    generated_calls: u64,
    generated_call_deopts: u64,
    compile_attempts: u64,
    code_generations: u64,
    minor_gc_cycles: u64,
}

impl CounterDelta {
    fn between(before: RuntimeExecutionStats, after: RuntimeExecutionStats) -> Self {
        Self {
            optimized_entries: after.jit_optimized_entries - before.jit_optimized_entries,
            optimized_deopts: after.jit_optimized_deopts - before.jit_optimized_deopts,
            generated_calls: after.jit_generated_calls - before.jit_generated_calls,
            generated_call_deopts: after.jit_generated_call_deopts
                - before.jit_generated_call_deopts,
            compile_attempts: after.jit_compile_attempts - before.jit_compile_attempts,
            code_generations: after.jit_code_generations - before.jit_code_generations,
            minor_gc_cycles: after.gc_minor_cycles - before.gc_minor_cycles,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MethodCandidateArtifact {
    index: u64,
    count: u64,
    function_id: u64,
}

fn runtime(selection: JitSelection, artifacts: bool) -> Runtime {
    let builder = Runtime::builder().jit_selection(selection);
    if artifacts {
        builder.jit_debug(JitDebugRequest::artifacts()).build()
    } else {
        builder.build()
    }
    .expect("Machine call-chain runtime")
}

fn completion(runtime: &mut Runtime, source: impl Into<String>, module: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source.into()), module)
        .unwrap_or_else(|error| panic!("Machine call-chain fixture {module}: {error:?}"))
        .completion_string()
        .to_owned()
}

fn artifact_json(bundle: &JitArtifactBundle, file: JitArtifactFileName) -> serde_json::Value {
    serde_json::from_slice(
        bundle
            .file(file)
            .unwrap_or_else(|| panic!("missing {file:?} in {:?}", bundle.manifest()))
            .contents(),
    )
    .unwrap_or_else(|error| panic!("invalid {file:?} JSON: {error}"))
}

fn is_exact_machine_bundle(bundle: &JitArtifactBundle, module: &str, function_name: &str) -> bool {
    let manifest = bundle.manifest();
    manifest.module() == module
        && manifest.function_name() == function_name
        && manifest.tier() == JitDebugTier::Optimizing
        && bundle
            .file(JitArtifactFileName::OptimizedIr)
            .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
}

fn has_optimizing_function_artifact(
    artifacts: Option<&JitArtifactBatch>,
    module: &str,
    function_name: &str,
) -> bool {
    artifacts.is_some_and(|artifacts| {
        artifacts.bundles().iter().any(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == module
                && manifest.function_name() == function_name
                && manifest.tier() == JitDebugTier::Optimizing
        })
    })
}

fn gc_stress_stride() -> u32 {
    let Ok(value) = std::env::var("OTTER_GC_STRESS") else {
        return 0;
    };
    let value = value.trim().to_ascii_lowercase();
    if value == "full" || value.is_empty() {
        return 1;
    }
    value
        .trim_end_matches("full")
        .trim_end_matches(['=', ',', ':'])
        .trim()
        .parse::<u32>()
        .unwrap_or(1)
}

fn method_candidates(bundle: &JitArtifactBundle) -> Vec<MethodCandidateArtifact> {
    let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
    let mut candidates = relocations["relocations"]
        .as_array()
        .expect("relocation array")
        .iter()
        .filter_map(|relocation| {
            let target = &relocation["target"];
            let direct = &target["directCall"];
            (target["kind"] == "directCallEntryCell" && direct["callKind"] == "method").then(|| {
                MethodCandidateArtifact {
                    index: direct["targetIndex"]
                        .as_u64()
                        .expect("method candidate targetIndex"),
                    count: direct["targetCount"]
                        .as_u64()
                        .expect("method candidate targetCount"),
                    function_id: direct["targetFunctionId"]
                        .as_u64()
                        .expect("method candidate function id"),
                }
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| candidate.index);
    candidates
}

fn assert_four_candidate_machine_chain(
    artifacts: &JitArtifactBatch,
    module: &str,
    function_name: &str,
    expect_shared_first_and_last: bool,
) {
    let mut observed = Vec::new();
    let matching_backends = artifacts
        .bundles()
        .iter()
        .filter(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == module
                && manifest.function_name() == function_name
                && manifest.tier() == JitDebugTier::Optimizing
        })
        .map(|bundle| {
            bundle
                .file(JitArtifactFileName::OptimizedIr)
                .and_then(|file| std::str::from_utf8(file.contents()).ok())
                .and_then(|text| text.lines().next())
                .unwrap_or("<missing optimized IR>")
                .to_owned()
        })
        .collect::<Vec<_>>();
    for bundle in artifacts
        .bundles()
        .iter()
        .filter(|bundle| is_exact_machine_bundle(bundle, module, function_name))
    {
        let candidates = method_candidates(bundle);
        observed.push(candidates.clone());
        if candidates.len() != 4 {
            continue;
        }
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (candidate.index, candidate.count))
                .collect::<Vec<_>>(),
            vec![(0, 4), (1, 4), (2, 4), (3, 4)],
            "method candidates must describe one dense complete chain"
        );
        if expect_shared_first_and_last {
            assert_eq!(
                candidates[0].function_id, candidates[3].function_id,
                "two shapes sharing one body must retain separate `this` guards"
            );
        }

        let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
        let regions = code_map["regions"].as_array().expect("code-map regions");
        for kind in ["machineDirectMethodGuard", "machineDirectMethodCandidate"] {
            assert_eq!(
                regions
                    .iter()
                    .filter(|region| region["kind"] == kind)
                    .count(),
                4,
                "{function_name} must expose every {kind}: {code_map}"
            );
        }
        return;
    }
    panic!(
        "missing four-candidate Machine chain for {module}:{function_name}; \
         observed={observed:?}; backends={matching_backends:?}"
    );
}

fn assert_single_machine_cold_exit(
    artifacts: &JitArtifactBatch,
    module: &str,
    function_name: &str,
) {
    let mut observed = Vec::new();
    for bundle in artifacts
        .bundles()
        .iter()
        .filter(|bundle| is_exact_machine_bundle(bundle, module, function_name))
    {
        let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
        let regions = code_map["regions"].as_array().expect("code-map regions");
        let cold_exits = regions
            .iter()
            .filter(|region| region["kind"] == "machineColdCallExit")
            .count();
        observed.push(cold_exits);
        if cold_exits != 1 {
            continue;
        }
        let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
        assert!(
            relocations["relocations"]
                .as_array()
                .expect("relocation array")
                .iter()
                .all(|relocation| relocation["target"]["kind"] != "directCallEntryCell"),
            "an unseen call must not fabricate a direct target: {relocations}"
        );
        return;
    }
    panic!("missing exact Machine cold exit for {module}:{function_name}; observed={observed:?}");
}

fn assert_parse_int_leaf_is_not_a_cold_exit(
    artifacts: &JitArtifactBatch,
    module: &str,
    function_name: &str,
) {
    let mut observed = Vec::new();
    for bundle in artifacts.bundles().iter().filter(|bundle| {
        let manifest = bundle.manifest();
        manifest.module() == module
            && manifest.function_name() == function_name
            && manifest.tier() == JitDebugTier::Optimizing
    }) {
        let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
        let regions = code_map["regions"].as_array().expect("code-map regions");
        let cold_exits = regions
            .iter()
            .filter(|region| region["kind"] == "machineColdCallExit")
            .count();
        let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
        let parse_leaf = relocations["relocations"]
            .as_array()
            .expect("relocation array")
            .iter()
            .any(|relocation| {
                relocation["target"]["kind"] == "runtimeStub"
                    && relocation["target"]["name"] == "parse_int_i32_leaf"
            });
        observed.push((cold_exits, parse_leaf));
        if cold_exits != 0 || !parse_leaf {
            continue;
        }
        let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
        assert!(
            relocations["relocations"]
                .as_array()
                .expect("relocation array")
                .iter()
                .any(|relocation| {
                    relocation["target"]["kind"] == "runtimeStub"
                        && relocation["target"]["name"] == "parse_int_i32_leaf"
                }),
            "parseInt must retain its declared leaf relocation: {relocations}"
        );
        return;
    }
    panic!(
        "parseInt was lost or misclassified as a cold exit for {module}:{function_name}; \
         observed={observed:?}"
    );
}

fn run_oracle_sequence(setup: &str, probes: &[(&str, &str)]) -> Vec<String> {
    let mut oracle = runtime(JitSelection::InterpreterOnly, false);
    completion(&mut oracle, setup, "jit-machine-call-chain-oracle-setup.js");
    probes
        .iter()
        .map(|(source, module)| completion(&mut oracle, *source, module))
        .collect()
}

#[test]
fn four_shape_chain_stays_generated_and_abrupt_candidates_do_not_replay() {
    let oracle = run_oracle_sequence(
        METHOD_CHAIN_SETUP,
        &[
            (METHOD_CHAIN_SAFE, "jit-machine-chain-safe-oracle.js"),
            (METHOD_CHAIN_ABRUPT, "jit-machine-chain-abrupt-oracle.js"),
        ],
    );

    let mut compiled = runtime(JitSelection::ProductionTiered, true);
    let setup = compiled
        .run_script(
            SourceInput::from_javascript(METHOD_CHAIN_SETUP),
            "jit-machine-chain-setup.js",
        )
        .expect("four-shape Machine method-chain setup");
    assert_four_candidate_machine_chain(
        setup.jit_artifacts().expect("method-chain artifacts"),
        "jit-machine-chain-setup.js",
        "machineMethodChain",
        true,
    );
    drop(setup);

    let before_safe = compiled.execution_stats();
    let safe = completion(
        &mut compiled,
        METHOD_CHAIN_SAFE,
        "jit-machine-chain-safe.js",
    );
    let safe_delta = CounterDelta::between(before_safe, compiled.execution_stats());
    assert_eq!(safe, oracle[0]);
    assert_eq!(safe, "[101,3,303,404,107,407]");
    assert!(safe_delta.optimized_entries >= 6, "{safe_delta:?}");
    assert!(safe_delta.generated_calls >= 6, "{safe_delta:?}");
    assert_eq!(
        safe_delta.optimized_deopts, 0,
        "all four guarded targets must stay in the complete outer Machine body: {safe_delta:?}"
    );

    let before_abrupt = compiled.execution_stats();
    let abrupt = completion(
        &mut compiled,
        METHOD_CHAIN_ABRUPT,
        "jit-machine-chain-abrupt.js",
    );
    let abrupt_delta = CounterDelta::between(before_abrupt, compiled.execution_stats());
    assert_eq!(abrupt, oracle[1]);
    assert_eq!(abrupt, r#"[2147483648,1,"chain-boom",1,142]"#);
    assert!(
        abrupt_delta.generated_call_deopts >= 1,
        "overflow must resume an already-started candidate: {abrupt_delta:?}"
    );
    assert!(abrupt_delta.generated_calls >= 3, "{abrupt_delta:?}");
    assert_eq!(
        abrupt_delta.optimized_deopts, abrupt_delta.generated_call_deopts,
        "only the already-started callee may deopt; the Machine caller must not replay it: \
         {abrupt_delta:?}"
    );
}

#[test]
fn fifth_shape_completes_generically_without_replacing_the_caller() {
    let oracle = run_oracle_sequence(
        METHOD_CHAIN_SETUP,
        &[
            (
                METHOD_CHAIN_FIFTH_ONCE,
                "jit-machine-chain-fifth-once-oracle.js",
            ),
            (
                METHOD_CHAIN_FIFTH_RECOMPILE,
                "jit-machine-chain-fifth-recompile-oracle.js",
            ),
        ],
    );

    let mut compiled = runtime(JitSelection::ProductionTiered, true);
    let setup = compiled
        .run_script(
            SourceInput::from_javascript(METHOD_CHAIN_SETUP),
            "jit-machine-chain-fifth-setup.js",
        )
        .expect("fifth-shape Machine method-chain setup");
    assert_four_candidate_machine_chain(
        setup.jit_artifacts().expect("fifth-shape setup artifacts"),
        "jit-machine-chain-fifth-setup.js",
        "machineMethodChain",
        true,
    );
    drop(setup);

    let before_fifth = compiled.execution_stats();
    let first_result = compiled
        .run_script(
            SourceInput::from_javascript(METHOD_CHAIN_FIFTH_ONCE),
            "jit-machine-chain-fifth-once.js",
        )
        .expect("first fifth-shape call");
    let first_recompiled_caller = has_optimizing_function_artifact(
        first_result.jit_artifacts(),
        "jit-machine-chain-fifth-setup.js",
        "machineMethodChain",
    );
    let first = first_result.completion_string().to_owned();
    drop(first_result);
    let after_first = compiled.execution_stats();
    let first_delta = CounterDelta::between(before_fifth, after_first);
    assert_eq!(first, oracle[0]);
    assert_eq!(first, "[505,1]");
    assert_eq!(
        first_delta.optimized_deopts, 0,
        "the unseen fifth guard must complete via the committed generic sibling: {first_delta:?}"
    );

    let rebuild_result = compiled
        .run_script(
            SourceInput::from_javascript(METHOD_CHAIN_FIFTH_RECOMPILE),
            "jit-machine-chain-fifth-recompile.js",
        )
        .expect("fifth-shape caller recompile probe");
    let second_recompiled_caller = has_optimizing_function_artifact(
        rebuild_result.jit_artifacts(),
        "jit-machine-chain-fifth-setup.js",
        "machineMethodChain",
    );
    let rebuilt = rebuild_result.completion_string().to_owned();
    drop(rebuild_result);
    let after_rebuild = compiled.execution_stats();
    let rebuild_delta = CounterDelta::between(before_fifth, after_rebuild);
    let post_first_delta = CounterDelta::between(after_first, after_rebuild);
    assert_eq!(rebuilt, oracle[1]);
    assert_eq!(rebuilt, "[128896,256,109]");
    assert!(rebuild_delta.compile_attempts > 0, "{rebuild_delta:?}");
    assert!(rebuild_delta.code_generations > 0, "{rebuild_delta:?}");
    assert!(
        !first_recompiled_caller && !second_recompiled_caller,
        "the generic fifth target must preserve the caller's four generated candidates"
    );
    assert_eq!(
        post_first_delta.optimized_deopts, 0,
        "the rebuilt five-shape caller must not remain in an outer deopt loop: {post_first_delta:?}"
    );
}

#[test]
fn unseen_plain_and_method_calls_are_exact_cold_exits_but_parse_int_stays_a_leaf() {
    let oracle = run_oracle_sequence(
        COLD_CALL_SETUP,
        &[
            (COLD_CALL_HOT_PROBE, "jit-machine-cold-hot-oracle.js"),
            (COLD_PLAIN_PROBE, "jit-machine-cold-plain-oracle.js"),
            (COLD_METHOD_PROBE, "jit-machine-cold-method-oracle.js"),
        ],
    );

    let mut compiled = runtime(JitSelection::ProductionTiered, true);
    let setup = compiled
        .run_script(
            SourceInput::from_javascript(COLD_CALL_SETUP),
            "jit-machine-cold-call-setup.js",
        )
        .expect("Machine cold-call setup");
    let artifacts = setup.jit_artifacts().expect("cold-call artifacts");
    assert_single_machine_cold_exit(
        artifacts,
        "jit-machine-cold-call-setup.js",
        "machineColdPlain",
    );
    assert_single_machine_cold_exit(
        artifacts,
        "jit-machine-cold-call-setup.js",
        "machineColdMethod",
    );
    assert_parse_int_leaf_is_not_a_cold_exit(
        artifacts,
        "jit-machine-cold-call-setup.js",
        "parseIntColdExitControl",
    );
    drop(setup);

    let before_hot = compiled.execution_stats();
    let hot = completion(
        &mut compiled,
        COLD_CALL_HOT_PROBE,
        "jit-machine-cold-call-hot.js",
    );
    let hot_delta = CounterDelta::between(before_hot, compiled.execution_stats());
    assert_eq!(hot, oracle[0]);
    assert_eq!(hot, "[42,43,41]");
    assert!(hot_delta.optimized_entries >= 3, "{hot_delta:?}");
    assert_eq!(hot_delta.optimized_deopts, 0, "{hot_delta:?}");

    let before_plain = compiled.execution_stats();
    let plain = completion(&mut compiled, COLD_PLAIN_PROBE, "jit-machine-cold-plain.js");
    let plain_delta = CounterDelta::between(before_plain, compiled.execution_stats());
    assert_eq!(plain, oracle[1]);
    assert_eq!(plain, "[51,1]");
    assert_eq!(plain_delta.optimized_deopts, 1, "{plain_delta:?}");

    let before_method = compiled.execution_stats();
    let method = completion(
        &mut compiled,
        COLD_METHOD_PROBE,
        "jit-machine-cold-method.js",
    );
    let method_delta = CounterDelta::between(before_method, compiled.execution_stats());
    assert_eq!(method, oracle[2]);
    assert_eq!(method, "[48,1]");
    assert_eq!(method_delta.optimized_deopts, 1, "{method_delta:?}");
}

#[test]
fn method_candidates_two_through_four_keep_receiver_and_arguments_rooted() {
    let stress_stride = gc_stress_stride();
    let allocation_count = if stress_stride == 0 {
        200_000
    } else {
        stress_stride
            .saturating_mul(2)
            .saturating_add(2)
            .min(200_000)
    };

    let mut compiled = runtime(JitSelection::ProductionTiered, true);
    let setup = compiled
        .run_script(
            SourceInput::from_javascript(METHOD_CHAIN_GC_SETUP),
            "jit-machine-chain-gc-setup.js",
        )
        .expect("Machine method-chain GC setup");
    assert_four_candidate_machine_chain(
        setup.jit_artifacts().expect("method-chain GC artifacts"),
        "jit-machine-chain-gc-setup.js",
        "machineMethodChainGc",
        false,
    );
    drop(setup);

    for (candidate, argument, expected_result) in [(1, 11, 211), (2, 22, 322), (3, 33, 433)] {
        compiled
            .force_gc()
            .unwrap_or_else(|error| panic!("age candidate {candidate} receiver: {error:?}"));
        let before = compiled.execution_stats();
        let probe = format!(
            r#"
(() => {{
  const argument = {{ value: {argument} }};
  const result = machineMethodChainGc(
    __machineChainGcReceivers[{candidate}],
    argument,
    {allocation_count}
  );
  const lastIndex = __machineChainGcSink.index;
  globalThis.__machineChainGcSink = null;
  return JSON.stringify([result, lastIndex]);
}})();
"#
        );
        let module = format!("jit-machine-chain-gc-candidate-{candidate}.js");
        let result = completion(&mut compiled, probe, &module);
        let delta = CounterDelta::between(before, compiled.execution_stats());
        assert_eq!(
            result,
            format!("[{expected_result},{}]", allocation_count - 1)
        );
        assert!(
            delta.optimized_entries >= 1,
            "candidate {candidate}: {delta:?}"
        );
        assert!(
            delta.generated_calls >= 1,
            "candidate {candidate}: {delta:?}"
        );
        assert!(
            delta.minor_gc_cycles > 0,
            "candidate {candidate} must collect while its receiver/argument are live: {delta:?}"
        );
    }

    compiled
        .force_gc()
        .expect("completed method candidates must unlink their root records");
    let reused = completion(
        &mut compiled,
        "machineMethodChainGc(__machineChainGcReceiver0, { value: 9 }, 0);",
        "jit-machine-chain-gc-reuse.js",
    );
    assert_eq!(reused, "109");
}

#[test]
fn compiled_generic_method_attempt_is_not_refrozen_as_a_cold_exit() {
    let mut compiled = runtime(JitSelection::ProductionTiered, true);
    let result = compiled
        .run_script(
            SourceInput::from_javascript(COMPILED_GENERIC_METHOD_ATTEMPT),
            "jit-compiled-generic-method-attempt.js",
        )
        .expect("compiled generic method-attempt fixture");
    assert_eq!(result.completion_string(), "[42,1]");
    assert!(
        result.stats().jit_reentrant_stub_transitions > 0,
        "the first method attempt must execute through the compiled generic boundary: {:?}",
        result.stats()
    );

    let artifacts = result.jit_artifacts().expect("method-attempt artifacts");
    let optimizing = artifacts
        .bundles()
        .iter()
        .filter(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == "jit-compiled-generic-method-attempt.js"
                && manifest.function_name() == "compiledGenericMethodAttempt"
                && manifest.tier() == JitDebugTier::Optimizing
        })
        .collect::<Vec<_>>();
    assert!(
        !optimizing.is_empty(),
        "fixture must reach optimizing compilation: {artifacts:?}"
    );
    for bundle in optimizing {
        let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
        let regions = code_map["regions"].as_array().expect("code-map regions");
        assert!(
            regions
                .iter()
                .all(|region| region["kind"] != "machineColdCallExit"),
            "an attempted method must not be refrozen as a cold exit: {code_map}"
        );
        assert!(
            regions
                .iter()
                .any(|region| region["kind"] == "machineGenericMethodCall"
                    && region["bytePc"].is_u64()),
            "the attempted method must retain its committed generic call: {code_map}"
        );
    }
}
