//! Machine IR generic computed-element value-call coverage.
//!
//! # Contents
//! - One mixed function whose prepared packed-double access is expressed as
//!   view/address/value nodes while cold computed accesses use the same
//!   committed reentrant value boundary.
//! - Ordinary-object, proxy, and observable key-coercion probes with exact
//!   effect and runtime-transition counts across feedback maturation.
//! - A local `try`/`catch` fixture proving the committed Machine throw edge
//!   reaches the JavaScript catch exactly once.
//!
//! # Invariants
//! - A never-taken generic branch does not deopt or enter either element stub.
//! - A taken generic load/store executes each coercion, proxy trap, and store
//!   exactly once; it never reconstructs and replays the source bytecode.
//! - Element-family misses remain in the explicit committed cold sibling and
//!   do not cause a compile or deopt storm.
//! - Local catches are never bypassed by a propagating Machine runtime call.
//!
//! # See also
//! - `crates/otter-jit/src/machine/numeric` owns generic element HIR,
//!   selection, safepoints, and AArch64 emission.
//! - `crates/otter-vm/src/runtime_activation/value_ops.rs` owns the stack-owned
//!   value-call boundary used by generated code.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTier,
    JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";
const MIXED_MODULE: &str = "jit-machine-generic-elements-setup.js";
const MIXED_FUNCTION: &str = "machineGenericElementBoundary";
const CATCH_MODULE: &str = "jit-machine-generic-elements-catch-setup.js";
const CATCH_FUNCTION: &str = "machineGenericElementCaught";

const MIXED_SETUP: &str = r#"
function machineGenericElementBoundary(values, takeCold, target, key, next) {
  const hot = values[0] + 0.5;
  values[0] = hot;
  if (takeCold) {
    const previous = target[key];
    target[key] = next;
    return previous + hot;
  }
  return hot;
}

globalThis.__machineGenericValues = [1.25, 2.5];
for (let warm = 0; warm < 5000; warm++) {
  machineGenericElementBoundary(
    __machineGenericValues,
    false,
    undefined,
    undefined,
    undefined
  );
}
"#;

const HOT_ONLY: &str = r#"
machineGenericElementBoundary(
  __machineGenericValues,
  false,
  undefined,
  undefined,
  undefined
);
"#;

const ORDINARY_PROBE: &str = r#"
globalThis.__machineGenericOrdinaryEffects = { keyCalls: 0 };
globalThis.__machineGenericOrdinary = { slot: 7 };
globalThis.__machineGenericOrdinaryKey = {
  toString() {
    __machineGenericOrdinaryEffects.keyCalls++;
    globalThis.__machineGenericOrdinaryGarbage = {
      call: __machineGenericOrdinaryEffects.keyCalls
    };
    return "slot";
  }
};
globalThis.__machineGenericOrdinaryResult = machineGenericElementBoundary(
  __machineGenericValues,
  true,
  __machineGenericOrdinary,
  __machineGenericOrdinaryKey,
  41
);
JSON.stringify([
  __machineGenericOrdinaryResult,
  __machineGenericOrdinary.slot,
  __machineGenericOrdinaryEffects.keyCalls
]);
"#;

const REBUILD_HOT_ONLY: &str = r#"
let __machineGenericRebuildTotal = 0;
for (let round = 0; round < 256; round++) {
  __machineGenericRebuildTotal += machineGenericElementBoundary(
    __machineGenericValues,
    false,
    undefined,
    undefined,
    undefined
  );
}
String(__machineGenericRebuildTotal);
"#;

const PROXY_SETUP: &str = r#"
globalThis.__machineGenericProxyEffects = {
  keyCalls: 0,
  getCalls: 0,
  setCalls: 0
};
globalThis.__machineGenericProxyTarget = { slot: 9 };
globalThis.__machineGenericProxy = new Proxy(__machineGenericProxyTarget, {
  get(target, property, receiver) {
    __machineGenericProxyEffects.getCalls++;
    return Reflect.get(target, property, receiver);
  },
  set(target, property, value, receiver) {
    __machineGenericProxyEffects.setCalls++;
    return Reflect.set(target, property, value, receiver);
  }
});
globalThis.__machineGenericProxyKey = {
  toString() {
    __machineGenericProxyEffects.keyCalls++;
    globalThis.__machineGenericProxyGarbage = {
      call: __machineGenericProxyEffects.keyCalls,
      value: __machineGenericProxyTarget.slot
    };
    return "slot";
  }
};
"#;

const PROXY_PROBE: &str = r#"
globalThis.__machineGenericProxyResult = machineGenericElementBoundary(
  __machineGenericValues,
  true,
  __machineGenericProxy,
  __machineGenericProxyKey,
  42
);
JSON.stringify([
  __machineGenericProxyResult,
  __machineGenericProxyTarget.slot,
  __machineGenericProxyEffects.keyCalls,
  __machineGenericProxyEffects.getCalls,
  __machineGenericProxyEffects.setCalls
]);
"#;

const PROXY_REUSE: &str = r#"
globalThis.__machineGenericProxyReuseResult = machineGenericElementBoundary(
  __machineGenericValues,
  true,
  __machineGenericProxy,
  __machineGenericProxyKey,
  43
);
JSON.stringify([
  __machineGenericProxyReuseResult,
  __machineGenericProxyTarget.slot,
  __machineGenericProxyEffects.keyCalls,
  __machineGenericProxyEffects.getCalls,
  __machineGenericProxyEffects.setCalls
]);
"#;

const CATCH_SETUP: &str = r#"
function machineGenericElementCaught(target, key) {
  try {
    return target[key];
  } catch (error) {
    return "caught:" + error.message;
  }
}

globalThis.__machineGenericCatchWarm = { slot: 3 };
for (let warm = 0; warm < 5000; warm++) {
  machineGenericElementCaught(__machineGenericCatchWarm, "slot");
}
"#;

const CATCH_PROBE: &str = r#"
globalThis.__machineGenericThrowingKeyCalls = 0;
globalThis.__machineGenericThrowingKey = {
  toString() {
    __machineGenericThrowingKeyCalls++;
    throw new Error("key-coercion");
  }
};
JSON.stringify([machineGenericElementCaught(
  __machineGenericCatchWarm,
  __machineGenericThrowingKey
), __machineGenericThrowingKeyCalls]);
"#;

#[derive(Clone, Copy, Debug)]
struct CounterDelta {
    optimized_entries: u64,
    optimized_deopts: u64,
    runtime_property_stubs: u64,
    reentrant_stub_transitions: u64,
    compile_attempts: u64,
    code_generations: u64,
}

impl CounterDelta {
    fn between(before: RuntimeExecutionStats, after: RuntimeExecutionStats) -> Self {
        Self {
            optimized_entries: after.jit_optimized_entries - before.jit_optimized_entries,
            optimized_deopts: after.jit_optimized_deopts - before.jit_optimized_deopts,
            runtime_property_stubs: after.jit_runtime_property_stubs
                - before.jit_runtime_property_stubs,
            reentrant_stub_transitions: after.jit_reentrant_stub_transitions
                - before.jit_reentrant_stub_transitions,
            compile_attempts: after.jit_compile_attempts - before.jit_compile_attempts,
            code_generations: after.jit_code_generations - before.jit_code_generations,
        }
    }
}

fn runtime(selection: JitSelection, artifacts: bool) -> Runtime {
    let builder = Runtime::builder().jit_selection(selection);
    if artifacts {
        builder
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .build()
    } else {
        builder.build()
    }
    .expect("generic element runtime")
}

fn completion(runtime: &mut Runtime, source: &str, module: &str) -> String {
    let result = runtime
        .run_script(SourceInput::from_javascript(source), module)
        .unwrap_or_else(|error| panic!("generic element script {module}: {error:?}"));
    result.completion_string().to_owned()
}

fn run_with_delta(runtime: &mut Runtime, source: &str, module: &str) -> (String, CounterDelta) {
    let before = runtime.execution_stats();
    let result = completion(runtime, source, module);
    let delta = CounterDelta::between(before, runtime.execution_stats());
    (result, delta)
}

fn artifact_json(bundle: &JitArtifactBundle, file: JitArtifactFileName) -> serde_json::Value {
    serde_json::from_slice(
        bundle
            .file(file)
            .unwrap_or_else(|| panic!("missing {file:?} artifact"))
            .contents(),
    )
    .unwrap_or_else(|error| panic!("invalid {file:?} JSON: {error}"))
}

fn mixed_machine_bundle(artifacts: &JitArtifactBatch) -> &JitArtifactBundle {
    artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == MIXED_MODULE
                && manifest.function_name() == MIXED_FUNCTION
                && manifest.tier() == JitDebugTier::Optimizing
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
        })
        .unwrap_or_else(|| {
            let manifests = artifacts
                .bundles()
                .iter()
                .map(|bundle| {
                    let manifest = bundle.manifest();
                    format!(
                        "{}:{}:{:?}",
                        manifest.module(),
                        manifest.function_name(),
                        manifest.tier()
                    )
                })
                .collect::<Vec<_>>();
            panic!("missing mixed generic-element Machine bundle: {manifests:?}")
        })
}

fn assert_mixed_machine_artifact(artifacts: &JitArtifactBatch) {
    let bundle = mixed_machine_bundle(artifacts);
    let optimized_ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("mixed generic-element optimized IR")
            .contents(),
    )
    .expect("UTF-8 mixed generic-element optimized IR");
    for opcode in [
        "ElementView",
        "ElementAddress",
        "ElementValueLoad",
        "ElementValueGuard",
        "ElementValueStore",
    ] {
        assert!(
            optimized_ir.contains(opcode),
            "mixed function must retain {opcode}: {optimized_ir}"
        );
    }
    for (stub_id, signature) in [
        (
            otter_vm::native_abi::STUB_JIT_LOAD_ELEMENT.id,
            "ReentrantValue2",
        ),
        (
            otter_vm::native_abi::STUB_JIT_STORE_ELEMENT.id,
            "ReentrantValue3",
        ),
    ] {
        assert!(
            optimized_ir.lines().any(|line| {
                line.contains(&format!("id: {stub_id}"))
                    && line.contains(&format!("signature: {signature}"))
            }),
            "mixed function must select stub {stub_id} with {signature}: {optimized_ir}"
        );
    }

    let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let regions = code_map["regions"].as_array().expect("code-map regions");
    for (kind, expected) in [
        ("machineElementView", 2),
        ("machineElementAddress", 2),
        ("machineElementValueLoad", 1),
        ("machineElementValueGuard", 1),
        ("machineElementValueStore", 1),
        ("machineCommittedValueEffect", 4),
    ] {
        let matching = regions
            .iter()
            .filter(|region| region["kind"] == kind)
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            expected,
            "mixed function must expose exactly {expected} {kind} regions: {code_map}"
        );
        assert!(
            matching[0]["bytePc"].as_u64().is_some(),
            "{kind} must retain source bytecode attribution: {code_map}"
        );
    }

    let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
    let relocations = relocations["relocations"]
        .as_array()
        .expect("relocation entries");
    for stub in ["jit_load_element", "jit_store_element"] {
        assert!(
            relocations.iter().any(|relocation| {
                relocation["target"]["kind"] == "runtimeStub"
                    && relocation["target"]["name"] == stub
            }),
            "mixed Machine body must retain the {stub} value call: {relocations:?}"
        );
    }

    let safepoints = artifact_json(bundle, JitArtifactFileName::Safepoints);
    assert!(
        safepoints["safepoints"]
            .as_array()
            .is_some_and(|safepoints| safepoints.len() >= 2),
        "both reentrant generic element calls must publish roots: {safepoints}"
    );
}

fn assert_exact_generic_pair(delta: CounterDelta, operation: &str) {
    assert!(
        delta.optimized_entries > 0,
        "{operation} must enter the Machine body: {delta:?}"
    );
    assert_eq!(
        delta.optimized_deopts, 0,
        "{operation} must complete without source-op replay: {delta:?}"
    );
    assert_eq!(
        delta.runtime_property_stubs, 2,
        "{operation} must execute one generic load and one generic store: {delta:?}"
    );
    assert_eq!(
        delta.reentrant_stub_transitions, 2,
        "{operation} must cross the reentrant value boundary exactly twice: {delta:?}"
    );
}

#[test]
fn cold_generic_value_calls_preserve_direct_hot_sites_and_execute_effects_once() {
    let mut runtime = runtime(JitSelection::ProductionTiered, true);
    let setup = runtime
        .run_script(SourceInput::from_javascript(MIXED_SETUP), MIXED_MODULE)
        .expect("mixed generic-element setup");
    assert_mixed_machine_artifact(
        setup
            .jit_artifacts()
            .expect("mixed generic-element artifacts"),
    );
    drop(setup);

    let (_, hot_delta) = run_with_delta(
        &mut runtime,
        HOT_ONLY,
        "jit-machine-generic-elements-hot-only.js",
    );
    assert!(hot_delta.optimized_entries > 0, "{hot_delta:?}");
    assert_eq!(hot_delta.optimized_deopts, 0, "{hot_delta:?}");
    assert_eq!(hot_delta.runtime_property_stubs, 0, "{hot_delta:?}");
    assert_eq!(hot_delta.reentrant_stub_transitions, 0, "{hot_delta:?}");

    let (ordinary, ordinary_delta) = run_with_delta(
        &mut runtime,
        ORDINARY_PROBE,
        "jit-machine-generic-elements-ordinary.js",
    );
    assert_eq!(ordinary, "[2509.25,41,2]");
    assert_exact_generic_pair(ordinary_delta, "ordinary computed access");
    assert_eq!(
        ordinary_delta.compile_attempts, 0,
        "feedback invalidation must not reenter compilation mid-call: {ordinary_delta:?}"
    );

    let (_, rebuild_delta) = run_with_delta(
        &mut runtime,
        REBUILD_HOT_ONLY,
        "jit-machine-generic-elements-rebuild.js",
    );
    assert_eq!(rebuild_delta.optimized_deopts, 0, "{rebuild_delta:?}");
    assert_eq!(rebuild_delta.runtime_property_stubs, 0, "{rebuild_delta:?}");
    assert_eq!(
        rebuild_delta.reentrant_stub_transitions, 0,
        "{rebuild_delta:?}"
    );
    assert_eq!(
        (
            rebuild_delta.compile_attempts,
            rebuild_delta.code_generations
        ),
        (0, 0),
        "the explicit committed cold sibling must remain reusable: {rebuild_delta:?}"
    );

    completion(
        &mut runtime,
        PROXY_SETUP,
        "jit-machine-generic-elements-proxy-setup.js",
    );
    let (proxy, proxy_delta) = run_with_delta(
        &mut runtime,
        PROXY_PROBE,
        "jit-machine-generic-elements-proxy.js",
    );
    assert_eq!(proxy, "[2639.75,42,2,1,1]");
    assert_exact_generic_pair(proxy_delta, "proxy computed access");
    assert_eq!(proxy_delta.compile_attempts, 0, "{proxy_delta:?}");
    assert_eq!(proxy_delta.code_generations, 0, "{proxy_delta:?}");

    let (reuse, reuse_delta) = run_with_delta(
        &mut runtime,
        PROXY_REUSE,
        "jit-machine-generic-elements-proxy-reuse.js",
    );
    assert_eq!(reuse, "[2673.25,43,4,2,2]");
    assert_exact_generic_pair(reuse_delta, "stable proxy computed access");
    assert_eq!(reuse_delta.compile_attempts, 0, "{reuse_delta:?}");
    assert_eq!(reuse_delta.code_generations, 0, "{reuse_delta:?}");
}

#[test]
fn generic_element_inside_local_catch_uses_machine_and_catches_key_throw() {
    let mut runtime = runtime(JitSelection::ProductionTiered, true);
    let setup = runtime
        .run_script(SourceInput::from_javascript(CATCH_SETUP), CATCH_MODULE)
        .expect("local-catch generic-element setup");
    let artifacts = setup
        .jit_artifacts()
        .expect("local-catch generic-element artifacts");
    let function_bundles = artifacts
        .bundles()
        .iter()
        .filter(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == CATCH_MODULE && manifest.function_name() == CATCH_FUNCTION
        })
        .collect::<Vec<_>>();
    assert!(
        function_bundles
            .iter()
            .any(|bundle| bundle.manifest().tier() == JitDebugTier::Template),
        "local-catch fixture must retain its Template baseline"
    );
    assert!(
        function_bundles.iter().any(|bundle| {
            bundle.manifest().tier() == JitDebugTier::Optimizing
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
        }),
        "the local catch must use the explicit Machine committed-throw CFG"
    );
    drop(setup);

    let (caught, delta) = run_with_delta(
        &mut runtime,
        CATCH_PROBE,
        "jit-machine-generic-elements-catch-probe.js",
    );
    assert_eq!(caught, r#"["caught:key-coercion",1]"#);
    assert_eq!(
        delta.runtime_property_stubs, 2,
        "the computed load and error.message read must each execute once: {delta:?}"
    );
    assert_eq!(
        delta.reentrant_stub_transitions, 2,
        "the computed load and error.message read must each cross the committed boundary once: \
         {delta:?}"
    );
}
