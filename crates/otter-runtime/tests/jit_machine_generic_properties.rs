//! Machine IR miss-capable named-property coverage.
//!
//! # Contents
//! - One mixed scalar function with settled hot property operations and cold
//!   named load/store sites backed by compiler-owned WhiskerIC cells.
//! - Ordinary shape reuse, an add-property transition, accessors, proxies, and
//!   allocating reentry under moving collection.
//! - Non-extensible and inline-capacity overflow receivers that stay on the
//!   canonical store boundary without object corruption.
//! - Same-layout descriptor invalidation after cell fill: a data load becomes
//!   an accessor and a writable store becomes non-writable.
//! - A local `try`/`catch` fixture that stays on Template until the property
//!   fast probe and committed cold call are explicit Machine CFG before
//!   register allocation.
//!
//! # Invariants
//! - A cold named-property site stays in the complete Machine body; its first
//!   ordinary miss calls the fixed boxed-value boundary exactly once and fills
//!   the code-owned cell, while the same shape reuses that cell without a stub,
//!   exact deopt, or replacement compilation.
//! - Getter, setter, and proxy effects execute exactly once. A committed store
//!   transition is never replayed, and every boxed operand/result remains a
//!   moving-GC root across reentrant calls.
//! - A cached add transition never bypasses receiver extensibility or an
//!   allocation-requiring slot-slab growth; those stores complete canonically
//!   once and leave all existing keys and values intact.
//! - Descriptor changes invalidate stale slot programs even if the receiver's
//!   shape token is otherwise reusable; accessors and rejected writes remain
//!   authoritative and execute exactly once.
//! - Named-property Machine regions retain bytecode attribution and stable
//!   IC-cell relocations; execution never takes an exact-deopt exit for the
//!   source property operations.
//!
//! # See also
//! - `crates/otter-jit/src/machine/numeric` owns property selection, cell probes,
//!   safepoints, and AArch64 emission.
//! - `crates/otter-vm/src/runtime_activation/value_ops.rs` owns the fixed value
//!   boundary used by generated named-property misses.

#![cfg(target_arch = "aarch64")]

use std::collections::BTreeSet;

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTier,
    JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";
const MIXED_MODULE: &str = "jit-machine-generic-properties-setup.js";
const MIXED_FUNCTION: &str = "machineGenericPropertyBoundary";
const CATCH_MODULE: &str = "jit-machine-generic-properties-catch-setup.js";
const CATCH_FUNCTION: &str = "machineGenericPropertyCaught";

const MIXED_SETUP: &str = r#"
function machineGenericPropertyBoundary(hot, takeCold, source, target, next) {
  const nextHot = hot.hot + 1;
  hot.hot = nextHot;
  if (takeCold) {
    const previous = source.payload;
    target.added = next;
    return previous;
  }
  return nextHot;
}

globalThis.__machineGenericPropertyHot = { hot: 0 };
for (let warm = 0; warm < 5000; warm++) {
  machineGenericPropertyBoundary(
    __machineGenericPropertyHot,
    false,
    undefined,
    undefined,
    undefined
  );
}
"#;

const HOT_ONLY: &str = r#"
machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  false,
  undefined,
  undefined,
  undefined
);
"#;

const ORDINARY_FIRST: &str = r#"
globalThis.__machineGenericPropertyFirstPrevious = { marker: 7 };
globalThis.__machineGenericPropertyFirstSource = {
  payload: __machineGenericPropertyFirstPrevious
};
globalThis.__machineGenericPropertyFirstTarget = Object.create(null);
__machineGenericPropertyFirstTarget.anchor = 1;
globalThis.__machineGenericPropertyFirstNext = { marker: 41 };
globalThis.__machineGenericPropertyFirstResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyFirstSource,
  __machineGenericPropertyFirstTarget,
  __machineGenericPropertyFirstNext
);
JSON.stringify([
  __machineGenericPropertyFirstResult === __machineGenericPropertyFirstPrevious,
  __machineGenericPropertyFirstResult.marker,
  __machineGenericPropertyFirstTarget.added === __machineGenericPropertyFirstNext,
  __machineGenericPropertyFirstTarget.added.marker,
  Object.keys(__machineGenericPropertyFirstTarget).join(","),
  __machineGenericPropertyHot.hot
]);
"#;

const ORDINARY_REUSE: &str = r#"
globalThis.__machineGenericPropertyReusePrevious = { marker: 8 };
globalThis.__machineGenericPropertyReuseSource = {
  payload: __machineGenericPropertyReusePrevious
};
globalThis.__machineGenericPropertyReuseTarget = Object.create(null);
__machineGenericPropertyReuseTarget.anchor = 2;
globalThis.__machineGenericPropertyReuseNext = { marker: 42 };
globalThis.__machineGenericPropertyReuseResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyReuseSource,
  __machineGenericPropertyReuseTarget,
  __machineGenericPropertyReuseNext
);
JSON.stringify([
  __machineGenericPropertyReuseResult === __machineGenericPropertyReusePrevious,
  __machineGenericPropertyReuseResult.marker,
  __machineGenericPropertyReuseTarget.added === __machineGenericPropertyReuseNext,
  __machineGenericPropertyReuseTarget.added.marker,
  Object.keys(__machineGenericPropertyReuseTarget).join(","),
  __machineGenericPropertyHot.hot
]);
"#;

const DEFAULT_PROTOTYPE_FIRST: &str = r#"
globalThis.__machineGenericPropertyDefaultFirstPrevious = { marker: 16 };
globalThis.__machineGenericPropertyDefaultFirstSource = {
  payload: __machineGenericPropertyDefaultFirstPrevious
};
globalThis.__machineGenericPropertyDefaultFirstTarget = { anchor: 16 };
globalThis.__machineGenericPropertyDefaultFirstNext = { marker: 49 };
globalThis.__machineGenericPropertyDefaultFirstResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyDefaultFirstSource,
  __machineGenericPropertyDefaultFirstTarget,
  __machineGenericPropertyDefaultFirstNext
);
JSON.stringify([
  __machineGenericPropertyDefaultFirstResult ===
    __machineGenericPropertyDefaultFirstPrevious,
  __machineGenericPropertyDefaultFirstResult.marker,
  __machineGenericPropertyDefaultFirstTarget.added ===
    __machineGenericPropertyDefaultFirstNext,
  __machineGenericPropertyDefaultFirstTarget.added.marker,
  Object.keys(__machineGenericPropertyDefaultFirstTarget).join(","),
  __machineGenericPropertyHot.hot
]);
"#;

const DEFAULT_PROTOTYPE_REUSE: &str = r#"
globalThis.__machineGenericPropertyDefaultReusePrevious = { marker: 17 };
globalThis.__machineGenericPropertyDefaultReuseSource = {
  payload: __machineGenericPropertyDefaultReusePrevious
};
globalThis.__machineGenericPropertyDefaultReuseTarget = { anchor: 17 };
globalThis.__machineGenericPropertyDefaultReuseNext = { marker: 50 };
globalThis.__machineGenericPropertyDefaultReuseResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyDefaultReuseSource,
  __machineGenericPropertyDefaultReuseTarget,
  __machineGenericPropertyDefaultReuseNext
);
JSON.stringify([
  __machineGenericPropertyDefaultReuseResult ===
    __machineGenericPropertyDefaultReusePrevious,
  __machineGenericPropertyDefaultReuseResult.marker,
  __machineGenericPropertyDefaultReuseTarget.added ===
    __machineGenericPropertyDefaultReuseNext,
  __machineGenericPropertyDefaultReuseTarget.added.marker,
  Object.keys(__machineGenericPropertyDefaultReuseTarget).join(","),
  __machineGenericPropertyHot.hot
]);
"#;

const NON_EXTENSIBLE_PROBE: &str = r#"
globalThis.__machineGenericPropertyFrozenPrevious = { marker: 9 };
globalThis.__machineGenericPropertyFrozenSource = {
  payload: __machineGenericPropertyFrozenPrevious
};
globalThis.__machineGenericPropertyFrozenTarget = Object.create(null);
__machineGenericPropertyFrozenTarget.anchor = 3;
Object.preventExtensions(__machineGenericPropertyFrozenTarget);
globalThis.__machineGenericPropertyFrozenNext = { marker: 43 };
globalThis.__machineGenericPropertyFrozenResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyFrozenSource,
  __machineGenericPropertyFrozenTarget,
  __machineGenericPropertyFrozenNext
);
JSON.stringify([
  __machineGenericPropertyFrozenResult === __machineGenericPropertyFrozenPrevious,
  __machineGenericPropertyFrozenResult.marker,
  Object.prototype.hasOwnProperty.call(
    __machineGenericPropertyFrozenTarget,
    "added"
  ),
  __machineGenericPropertyFrozenTarget.anchor,
  Object.keys(__machineGenericPropertyFrozenTarget).join(","),
  Object.isExtensible(__machineGenericPropertyFrozenTarget),
  __machineGenericPropertyHot.hot
]);
"#;

const OUT_OF_LINE_FIRST: &str = r#"
globalThis.__machineGenericPropertyOverflowFirstPrevious = { marker: 10 };
globalThis.__machineGenericPropertyOverflowFirstSource = {
  payload: __machineGenericPropertyOverflowFirstPrevious
};
globalThis.__machineGenericPropertyOverflowFirstTarget = Object.create(null);
__machineGenericPropertyOverflowFirstTarget.first = 1;
__machineGenericPropertyOverflowFirstTarget.second = 2;
__machineGenericPropertyOverflowFirstTarget.third = 3;
globalThis.__machineGenericPropertyOverflowFirstNext = { marker: 44 };
globalThis.__machineGenericPropertyOverflowFirstResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyOverflowFirstSource,
  __machineGenericPropertyOverflowFirstTarget,
  __machineGenericPropertyOverflowFirstNext
);
JSON.stringify([
  __machineGenericPropertyOverflowFirstResult ===
    __machineGenericPropertyOverflowFirstPrevious,
  __machineGenericPropertyOverflowFirstResult.marker,
  __machineGenericPropertyOverflowFirstTarget.added ===
    __machineGenericPropertyOverflowFirstNext,
  __machineGenericPropertyOverflowFirstTarget.first,
  __machineGenericPropertyOverflowFirstTarget.second,
  __machineGenericPropertyOverflowFirstTarget.third,
  __machineGenericPropertyOverflowFirstTarget.added.marker,
  Object.keys(__machineGenericPropertyOverflowFirstTarget).join(","),
  __machineGenericPropertyHot.hot
]);
"#;

const OUT_OF_LINE_REUSE: &str = r#"
globalThis.__machineGenericPropertyOverflowReusePrevious = { marker: 11 };
globalThis.__machineGenericPropertyOverflowReuseSource = {
  payload: __machineGenericPropertyOverflowReusePrevious
};
globalThis.__machineGenericPropertyOverflowReuseTarget = Object.create(null);
__machineGenericPropertyOverflowReuseTarget.first = 4;
__machineGenericPropertyOverflowReuseTarget.second = 5;
__machineGenericPropertyOverflowReuseTarget.third = 6;
globalThis.__machineGenericPropertyOverflowReuseNext = { marker: 45 };
globalThis.__machineGenericPropertyOverflowReuseResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyOverflowReuseSource,
  __machineGenericPropertyOverflowReuseTarget,
  __machineGenericPropertyOverflowReuseNext
);
JSON.stringify([
  __machineGenericPropertyOverflowReuseResult ===
    __machineGenericPropertyOverflowReusePrevious,
  __machineGenericPropertyOverflowReuseResult.marker,
  __machineGenericPropertyOverflowReuseTarget.added ===
    __machineGenericPropertyOverflowReuseNext,
  __machineGenericPropertyOverflowReuseTarget.first,
  __machineGenericPropertyOverflowReuseTarget.second,
  __machineGenericPropertyOverflowReuseTarget.third,
  __machineGenericPropertyOverflowReuseTarget.added.marker,
  Object.keys(__machineGenericPropertyOverflowReuseTarget).join(","),
  __machineGenericPropertyHot.hot
]);
"#;

const EXISTING_STORE_FILL: &str = r#"
globalThis.__machineGenericPropertyExistingPrevious = { marker: 12 };
globalThis.__machineGenericPropertyExistingSource = {
  payload: __machineGenericPropertyExistingPrevious
};
globalThis.__machineGenericPropertyExistingTarget = {
  added: { marker: 0 }
};
globalThis.__machineGenericPropertyExistingNext = { marker: 46 };
globalThis.__machineGenericPropertyExistingResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyExistingSource,
  __machineGenericPropertyExistingTarget,
  __machineGenericPropertyExistingNext
);
JSON.stringify([
  __machineGenericPropertyExistingResult ===
    __machineGenericPropertyExistingPrevious,
  __machineGenericPropertyExistingResult.marker,
  __machineGenericPropertyExistingTarget.added ===
    __machineGenericPropertyExistingNext,
  __machineGenericPropertyExistingTarget.added.marker,
  __machineGenericPropertyHot.hot
]);
"#;

const NON_WRITABLE_PROBE: &str = r#"
globalThis.__machineGenericPropertyReadonlyPrevious = { marker: 13 };
globalThis.__machineGenericPropertyReadonlySource = {
  payload: __machineGenericPropertyReadonlyPrevious
};
globalThis.__machineGenericPropertyReadonlyInitial = { marker: 90 };
globalThis.__machineGenericPropertyReadonlyTarget = {
  added: __machineGenericPropertyReadonlyInitial
};
Object.defineProperty(__machineGenericPropertyReadonlyTarget, "added", {
  writable: false
});
globalThis.__machineGenericPropertyReadonlyNext = { marker: 47 };
globalThis.__machineGenericPropertyReadonlyResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyReadonlySource,
  __machineGenericPropertyReadonlyTarget,
  __machineGenericPropertyReadonlyNext
);
globalThis.__machineGenericPropertyReadonlyDescriptor =
  Object.getOwnPropertyDescriptor(
    __machineGenericPropertyReadonlyTarget,
    "added"
  );
JSON.stringify([
  __machineGenericPropertyReadonlyResult ===
    __machineGenericPropertyReadonlyPrevious,
  __machineGenericPropertyReadonlyResult.marker,
  __machineGenericPropertyReadonlyTarget.added ===
    __machineGenericPropertyReadonlyInitial,
  __machineGenericPropertyReadonlyTarget.added.marker,
  __machineGenericPropertyReadonlyDescriptor.writable,
  __machineGenericPropertyHot.hot
]);
"#;

const ACCESSOR_INVALIDATION_PROBE: &str = r#"
globalThis.__machineGenericPropertyInvalidatedGetterCalls = 0;
globalThis.__machineGenericPropertyInvalidatedOld = { marker: 14 };
globalThis.__machineGenericPropertyInvalidatedGetterValue = { marker: 15 };
globalThis.__machineGenericPropertyInvalidatedSource = {
  payload: __machineGenericPropertyInvalidatedOld
};
Object.defineProperty(__machineGenericPropertyInvalidatedSource, "payload", {
  configurable: true,
  enumerable: true,
  get() {
    __machineGenericPropertyInvalidatedGetterCalls++;
    return __machineGenericPropertyInvalidatedGetterValue;
  }
});
globalThis.__machineGenericPropertyInvalidatedTarget = {
  added: { marker: 0 }
};
globalThis.__machineGenericPropertyInvalidatedNext = { marker: 48 };
globalThis.__machineGenericPropertyInvalidatedResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyInvalidatedSource,
  __machineGenericPropertyInvalidatedTarget,
  __machineGenericPropertyInvalidatedNext
);
JSON.stringify([
  __machineGenericPropertyInvalidatedResult ===
    __machineGenericPropertyInvalidatedGetterValue,
  __machineGenericPropertyInvalidatedResult.marker,
  __machineGenericPropertyInvalidatedTarget.added ===
    __machineGenericPropertyInvalidatedNext,
  __machineGenericPropertyInvalidatedTarget.added.marker,
  __machineGenericPropertyInvalidatedGetterCalls,
  __machineGenericPropertyHot.hot
]);
"#;

const ACCESSOR_PROBE: &str = r#"
globalThis.__machineGenericPropertyAccessorEffects = {
  getterCalls: 0,
  setterCalls: 0
};
globalThis.__machineGenericPropertyAccessorPrevious = { marker: 71 };
globalThis.__machineGenericPropertyAccessorSaved = null;
globalThis.__machineGenericPropertyAccessorSource = {};
Object.defineProperty(__machineGenericPropertyAccessorSource, "payload", {
  configurable: true,
  get() {
    __machineGenericPropertyAccessorEffects.getterCalls++;
    const garbage = [];
    for (let index = 0; index < 32; index++) {
      garbage.push({ index, text: "getter-garbage-" + index });
    }
    globalThis.__machineGenericPropertyAccessorGetterGarbage = garbage;
    return __machineGenericPropertyAccessorPrevious;
  }
});
globalThis.__machineGenericPropertyAccessorTarget = {};
Object.defineProperty(__machineGenericPropertyAccessorTarget, "added", {
  configurable: true,
  set(value) {
    __machineGenericPropertyAccessorEffects.setterCalls++;
    const garbage = [];
    for (let index = 0; index < 32; index++) {
      garbage.push({ index, text: "setter-garbage-" + index });
    }
    globalThis.__machineGenericPropertyAccessorSetterGarbage = garbage;
    __machineGenericPropertyAccessorSaved = value;
  }
});
globalThis.__machineGenericPropertyAccessorNext = { marker: 72 };
globalThis.__machineGenericPropertyAccessorResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyAccessorSource,
  __machineGenericPropertyAccessorTarget,
  __machineGenericPropertyAccessorNext
);
JSON.stringify([
  __machineGenericPropertyAccessorResult ===
    __machineGenericPropertyAccessorPrevious,
  __machineGenericPropertyAccessorResult.marker,
  __machineGenericPropertyAccessorSaved ===
    __machineGenericPropertyAccessorNext,
  __machineGenericPropertyAccessorSaved.marker,
  __machineGenericPropertyAccessorEffects.getterCalls,
  __machineGenericPropertyAccessorEffects.setterCalls
]);
"#;

const PROXY_PROBE: &str = r#"
globalThis.__machineGenericPropertyProxyEffects = {
  getCalls: 0,
  setCalls: 0
};
globalThis.__machineGenericPropertyProxyPrevious = { marker: 81 };
globalThis.__machineGenericPropertyProxySourceTarget = {
  payload: __machineGenericPropertyProxyPrevious
};
globalThis.__machineGenericPropertyProxySource = new Proxy(
  __machineGenericPropertyProxySourceTarget,
  {
    get(target, property, receiver) {
      __machineGenericPropertyProxyEffects.getCalls++;
      globalThis.__machineGenericPropertyProxyGetGarbage = {
        property,
        marker: __machineGenericPropertyProxyEffects.getCalls
      };
      return Reflect.get(target, property, receiver);
    }
  }
);
globalThis.__machineGenericPropertyProxyTarget = {};
globalThis.__machineGenericPropertyProxyReceiver = new Proxy(
  __machineGenericPropertyProxyTarget,
  {
    set(target, property, value, receiver) {
      __machineGenericPropertyProxyEffects.setCalls++;
      globalThis.__machineGenericPropertyProxySetGarbage = {
        property,
        marker: __machineGenericPropertyProxyEffects.setCalls
      };
      return Reflect.set(target, property, value, receiver);
    }
  }
);
globalThis.__machineGenericPropertyProxyNext = { marker: 82 };
globalThis.__machineGenericPropertyProxyResult = machineGenericPropertyBoundary(
  __machineGenericPropertyHot,
  true,
  __machineGenericPropertyProxySource,
  __machineGenericPropertyProxyReceiver,
  __machineGenericPropertyProxyNext
);
JSON.stringify([
  __machineGenericPropertyProxyResult === __machineGenericPropertyProxyPrevious,
  __machineGenericPropertyProxyResult.marker,
  __machineGenericPropertyProxyTarget.added ===
    __machineGenericPropertyProxyNext,
  __machineGenericPropertyProxyTarget.added.marker,
  __machineGenericPropertyProxyEffects.getCalls,
  __machineGenericPropertyProxyEffects.setCalls
]);
"#;

const CATCH_SETUP: &str = r#"
function machineGenericPropertyCaught(target) {
  try {
    return target.value;
  } catch (error) {
    return "caught:" + error.message;
  }
}

globalThis.__machineGenericPropertyCatchWarm = { value: 3 };
for (let warm = 0; warm < 5000; warm++) {
  machineGenericPropertyCaught(__machineGenericPropertyCatchWarm);
}
"#;

const CATCH_PROBE: &str = r#"
globalThis.__machineGenericPropertyCatchCalls = 0;
globalThis.__machineGenericPropertyThrowing = {};
Object.defineProperty(__machineGenericPropertyThrowing, "value", {
  configurable: true,
  get() {
    __machineGenericPropertyCatchCalls++;
    throw new Error("named-getter");
  }
});
JSON.stringify([
  machineGenericPropertyCaught(__machineGenericPropertyThrowing),
  __machineGenericPropertyCatchCalls
]);
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

fn runtime(artifacts: bool) -> Runtime {
    let builder = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_osr_threshold(u32::MAX);
    if artifacts {
        builder.jit_debug(JitDebugRequest::artifacts()).build()
    } else {
        builder.build()
    }
    .expect("generic named-property runtime")
}

fn completion(runtime: &mut Runtime, source: &str, module: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), module)
        .unwrap_or_else(|error| panic!("generic named-property script {module}: {error:?}"))
        .completion_string()
        .to_owned()
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
            panic!("missing mixed generic-property Machine bundle: {manifests:?}")
        })
}

fn assert_mixed_machine_artifact(artifacts: &JitArtifactBatch) {
    let bundle = mixed_machine_bundle(artifacts);
    let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let regions = code_map["regions"].as_array().expect("code-map regions");
    let mut property_byte_pcs = BTreeSet::new();
    for (kind, expected) in [
        ("machinePropertyLoad", 2usize),
        ("machinePropertyStore", 2usize),
    ] {
        let matching = regions
            .iter()
            .filter(|region| region["kind"] == kind)
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            expected,
            "mixed function must retain both settled and cold {kind} regions: {code_map}"
        );
        for region in matching {
            property_byte_pcs.insert(
                region["bytePc"].as_u64().unwrap_or_else(|| {
                    panic!("{kind} must retain bytecode attribution: {code_map}")
                }),
            );
        }
    }
    assert_eq!(
        property_byte_pcs.len(),
        4,
        "each named-property source operation needs a distinct byte PC: {code_map}"
    );

    let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
    let relocations = relocations["relocations"]
        .as_array()
        .expect("relocation entries");
    for (stub_id, stub, signature) in [
        (18u64, "jit_load_property_value", "reentrantNamedLoad"),
        (19u64, "jit_store_property_value", "reentrantNamedStore"),
    ] {
        assert!(
            relocations.iter().any(|relocation| {
                relocation["target"]["kind"] == "runtimeStub"
                    && relocation["target"]["id"].as_u64() == Some(stub_id)
                    && relocation["target"]["name"] == stub
                    && relocation["target"]["signature"] == signature
            }),
            "mixed Machine body must retain the {stub} fixed value call: {relocations:?}"
        );
    }
    for access in ["load", "store"] {
        assert!(
            relocations.iter().any(|relocation| {
                relocation["target"]["kind"] == "propertyIcCell"
                    && relocation["target"]["access"] == access
            }),
            "mixed Machine body must own a stable {access} WhiskerIC cell: {relocations:?}"
        );
    }

    let safepoints = artifact_json(bundle, JitArtifactFileName::Safepoints);
    assert!(
        safepoints["safepoints"]
            .as_array()
            .is_some_and(|safepoints| safepoints.len() >= 4),
        "all four miss-capable property operations must publish precise roots: {safepoints}"
    );
}

fn assert_machine_entry_without_deopt(delta: CounterDelta, operation: &str) {
    assert!(
        delta.optimized_entries > 0,
        "{operation} must enter the complete Machine body: {delta:?}"
    );
    assert_eq!(
        delta.optimized_deopts, 0,
        "{operation} must commit without source-op replay: {delta:?}"
    );
    assert_eq!(
        delta.compile_attempts, 0,
        "{operation} must not invalidate and synchronously replace the body: {delta:?}"
    );
    assert_eq!(
        delta.code_generations, 0,
        "{operation} must reuse the current Machine generation: {delta:?}"
    );
}

fn assert_exact_runtime_pair(delta: CounterDelta, operation: &str) {
    assert_machine_entry_without_deopt(delta, operation);
    assert_eq!(
        delta.runtime_property_stubs, 2,
        "{operation} must execute one named load and one named store boundary: {delta:?}"
    );
    assert_eq!(
        delta.reentrant_stub_transitions, 2,
        "{operation} must cross each fixed reentrant value boundary once: {delta:?}"
    );
}

#[test]
fn ordinary_misses_fill_code_owned_cells_and_same_shapes_reuse_without_stub() {
    let mut runtime = runtime(true);
    let setup = runtime
        .run_script(SourceInput::from_javascript(MIXED_SETUP), MIXED_MODULE)
        .expect("mixed generic-property setup");
    assert_mixed_machine_artifact(
        setup
            .jit_artifacts()
            .expect("mixed generic-property artifacts"),
    );
    drop(setup);

    let (_, hot_delta) = run_with_delta(
        &mut runtime,
        HOT_ONLY,
        "jit-machine-generic-properties-hot-only.js",
    );
    assert_machine_entry_without_deopt(hot_delta, "settled hot-only property path");
    assert_eq!(hot_delta.runtime_property_stubs, 0, "{hot_delta:?}");
    assert_eq!(hot_delta.reentrant_stub_transitions, 0, "{hot_delta:?}");

    let (first, first_delta) = run_with_delta(
        &mut runtime,
        ORDINARY_FIRST,
        "jit-machine-generic-properties-ordinary-first.js",
    );
    assert_eq!(first, r#"[true,7,true,41,"anchor,added",5002]"#);
    assert_exact_runtime_pair(first_delta, "first ordinary property miss pair");

    let (reuse, reuse_delta) = run_with_delta(
        &mut runtime,
        ORDINARY_REUSE,
        "jit-machine-generic-properties-ordinary-reuse.js",
    );
    assert_eq!(reuse, r#"[true,8,true,42,"anchor,added",5003]"#);
    assert_machine_entry_without_deopt(reuse_delta, "same-shape WhiskerIC reuse");
    assert_eq!(
        reuse_delta.runtime_property_stubs, 0,
        "same load shape and add transition must hit both code-owned cells: {reuse_delta:?}"
    );
    assert_eq!(
        reuse_delta.reentrant_stub_transitions, 0,
        "same-shape reuse must not enter either runtime boundary: {reuse_delta:?}"
    );

    let (frozen, frozen_delta) = run_with_delta(
        &mut runtime,
        NON_EXTENSIBLE_PROBE,
        "jit-machine-generic-properties-non-extensible.js",
    );
    assert_eq!(frozen, r#"[true,9,false,3,"anchor",false,5004]"#);
    assert_machine_entry_without_deopt(frozen_delta, "non-extensible add rejection");
    assert_eq!(
        frozen_delta.runtime_property_stubs, 1,
        "the cached transition must reject non-extensible receivers before the canonical store: \
         {frozen_delta:?}"
    );
    assert_eq!(
        frozen_delta.reentrant_stub_transitions, 1,
        "the rejected add must complete through exactly one store boundary: {frozen_delta:?}"
    );

    let (overflow_first, overflow_first_delta) = run_with_delta(
        &mut runtime,
        OUT_OF_LINE_FIRST,
        "jit-machine-generic-properties-overflow-first.js",
    );
    assert_eq!(
        overflow_first,
        r#"[true,10,true,1,2,3,44,"first,second,third,added",5005]"#
    );
    assert_machine_entry_without_deopt(
        overflow_first_delta,
        "inline-capacity overflow add transition",
    );
    assert_eq!(
        overflow_first_delta.runtime_property_stubs, 1,
        "slab growth must execute exactly one canonical store: {overflow_first_delta:?}"
    );
    assert_eq!(
        overflow_first_delta.reentrant_stub_transitions, 1,
        "slab growth must cross exactly one reentrant store boundary: \
         {overflow_first_delta:?}"
    );

    let (overflow_reuse, overflow_reuse_delta) = run_with_delta(
        &mut runtime,
        OUT_OF_LINE_REUSE,
        "jit-machine-generic-properties-overflow-reuse.js",
    );
    assert_eq!(
        overflow_reuse,
        r#"[true,11,true,4,5,6,45,"first,second,third,added",5006]"#
    );
    assert_machine_entry_without_deopt(
        overflow_reuse_delta,
        "repeated inline-capacity overflow add transition",
    );
    assert_eq!(
        overflow_reuse_delta.runtime_property_stubs, 1,
        "an allocation-requiring transition must remain on one canonical store per object: \
         {overflow_reuse_delta:?}"
    );
    assert_eq!(
        overflow_reuse_delta.reentrant_stub_transitions, 1,
        "the repeated growth path must not replay or enter twice: {overflow_reuse_delta:?}"
    );

    let (existing, existing_delta) = run_with_delta(
        &mut runtime,
        EXISTING_STORE_FILL,
        "jit-machine-generic-properties-existing-store-fill.js",
    );
    assert_eq!(existing, "[true,12,true,46,5007]");
    assert_machine_entry_without_deopt(existing_delta, "existing writable store cell fill");
    assert_eq!(
        existing_delta.runtime_property_stubs, 1,
        "the new existing-slot shape must fill one store cell way: {existing_delta:?}"
    );
    assert_eq!(
        existing_delta.reentrant_stub_transitions, 1,
        "the existing-slot fill must enter the store boundary once: {existing_delta:?}"
    );

    let (readonly, readonly_delta) = run_with_delta(
        &mut runtime,
        NON_WRITABLE_PROBE,
        "jit-machine-generic-properties-non-writable.js",
    );
    assert_eq!(readonly, "[true,13,true,90,false,5008]");
    assert_machine_entry_without_deopt(readonly_delta, "non-writable descriptor invalidation");
    assert_eq!(
        readonly_delta.runtime_property_stubs, 1,
        "a stale writable-slot cell must miss before the canonical rejected store: \
         {readonly_delta:?}"
    );
    assert_eq!(
        readonly_delta.reentrant_stub_transitions, 1,
        "the rejected non-writable store must execute exactly once: {readonly_delta:?}"
    );

    let (invalidated_load, invalidated_load_delta) = run_with_delta(
        &mut runtime,
        ACCESSOR_INVALIDATION_PROBE,
        "jit-machine-generic-properties-accessor-invalidation.js",
    );
    assert_eq!(invalidated_load, "[true,15,true,48,1,5009]");
    assert_machine_entry_without_deopt(
        invalidated_load_delta,
        "own-data to accessor load invalidation",
    );
    assert_eq!(
        invalidated_load_delta.runtime_property_stubs, 1,
        "a stale data-load cell must miss and invoke the getter canonically once: \
         {invalidated_load_delta:?}"
    );
    assert_eq!(
        invalidated_load_delta.reentrant_stub_transitions, 1,
        "the getter must be reached through one reentrant load boundary: \
         {invalidated_load_delta:?}"
    );
}

#[test]
fn default_object_prototype_adds_stay_on_one_canonical_store_per_peer() {
    let mut runtime = runtime(false);
    completion(&mut runtime, MIXED_SETUP, MIXED_MODULE);

    let (first, first_delta) = run_with_delta(
        &mut runtime,
        DEFAULT_PROTOTYPE_FIRST,
        "jit-machine-generic-properties-default-first.js",
    );
    assert_eq!(first, r#"[true,16,true,49,"anchor,added",5001]"#);
    assert_machine_entry_without_deopt(first_delta, "first default-prototype add");
    assert_eq!(
        first_delta.runtime_property_stubs, 2,
        "the first call needs one load fill plus one canonical default-prototype store: \
         {first_delta:?}"
    );
    assert_eq!(first_delta.reentrant_stub_transitions, 2, "{first_delta:?}");

    let (reuse, reuse_delta) = run_with_delta(
        &mut runtime,
        DEFAULT_PROTOTYPE_REUSE,
        "jit-machine-generic-properties-default-reuse.js",
    );
    assert_eq!(reuse, r#"[true,17,true,50,"anchor,added",5002]"#);
    assert_machine_entry_without_deopt(reuse_delta, "second default-prototype add");
    assert_eq!(
        reuse_delta.runtime_property_stubs, 1,
        "the load cell must hit while the unsupported default-prototype add executes once: \
         {reuse_delta:?}"
    );
    assert_eq!(reuse_delta.reentrant_stub_transitions, 1, "{reuse_delta:?}");
}

#[test]
fn accessor_and_proxy_effects_execute_once_and_preserve_moving_roots() {
    let mut runtime = runtime(false);
    completion(&mut runtime, MIXED_SETUP, MIXED_MODULE);

    let (accessor, accessor_delta) = run_with_delta(
        &mut runtime,
        ACCESSOR_PROBE,
        "jit-machine-generic-properties-accessor.js",
    );
    assert_eq!(accessor, "[true,71,true,72,1,1]");
    assert_exact_runtime_pair(accessor_delta, "allocating accessor pair");

    let (proxy, proxy_delta) = run_with_delta(
        &mut runtime,
        PROXY_PROBE,
        "jit-machine-generic-properties-proxy.js",
    );
    assert_eq!(proxy, "[true,81,true,82,1,1]");
    assert_exact_runtime_pair(proxy_delta, "proxy get/set pair");
}

#[test]
fn generic_named_property_inside_local_catch_remains_non_machine() {
    let mut runtime = runtime(true);
    let setup = runtime
        .run_script(SourceInput::from_javascript(CATCH_SETUP), CATCH_MODULE)
        .expect("local-catch generic-property setup");
    let artifacts = setup
        .jit_artifacts()
        .expect("local-catch generic-property artifacts");
    let function_bundles = artifacts
        .bundles()
        .iter()
        .filter(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == CATCH_MODULE && manifest.function_name() == CATCH_FUNCTION
        })
        .collect::<Vec<_>>();
    assert!(
        !function_bundles.is_empty(),
        "local-catch fixture must publish a materialized native artifact"
    );
    assert!(
        function_bundles.iter().all(|bundle| {
            bundle
                .file(JitArtifactFileName::OptimizedIr)
                .is_some_and(|file| !file.contents().starts_with(MACHINE_IR_HEADER))
                || bundle.file(JitArtifactFileName::OptimizedIr).is_none()
        }),
        "a propagating named-property Machine call must not bypass a local catch"
    );
    drop(setup);

    let (caught, delta) = run_with_delta(
        &mut runtime,
        CATCH_PROBE,
        "jit-machine-generic-properties-catch-probe.js",
    );
    assert_eq!(caught, r#"["caught:named-getter",1]"#);
    assert_eq!(
        delta.runtime_property_stubs, 1,
        "the throwing named load must execute exactly once: {delta:?}"
    );
}
