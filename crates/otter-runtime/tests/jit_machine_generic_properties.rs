//! Machine IR miss-capable named-property coverage.
//!
//! # Contents
//! - One mixed scalar function with immutable hot CacheIR programs and cold
//!   named load/store sites backed by compiler-owned source-identity cells.
//! - Ordinary shape reuse, an add-property transition, accessors, proxies, and
//!   allocating reentry under moving collection.
//! - Non-extensible and inline-capacity overflow receivers that stay on the
//!   canonical store boundary without object corruption.
//! - Same-layout descriptor invalidation after snapshot: a data load becomes
//!   an accessor and a writable store becomes non-writable.
//! - A store followed by two loads keeps the first tagged payload live across
//!   the second CacheIR probe's Boolean SSA plumbing.
//! - A local `try`/`catch` fixture whose throwing getter reaches the Machine
//!   landing pad through explicit committed status control without deopt.
//!
//! # Invariants
//! - A cold named-property site stays in the complete Machine body and calls
//!   the fixed boxed-value boundary exactly once per source operation. A
//!   published body never learns semantic proof data after compilation.
//! - Getter, setter, and proxy effects execute exactly once. A committed store
//!   transition is never replayed, and every boxed operand/result remains a
//!   moving-GC root across reentrant calls.
//! - A snapshot add transition never bypasses receiver extensibility or an
//!   allocation-requiring slot-slab growth; those stores complete canonically
//!   once and leave all existing keys and values intact.
//! - Descriptor changes invalidate stale slot programs even if the receiver's
//!   shape token is otherwise reusable; accessors and rejected writes remain
//!   authoritative and execute exactly once.
//! - Named-property Machine regions retain bytecode attribution and stable
//!   source-cell relocations; execution never takes an exact-deopt exit for the
//!   source property operations.
//!
//! # See also
//! - `crates/otter-jit/src/machine/numeric` owns property selection, CacheIR nodes,
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

const LIVE_PAYLOAD_WARM: &str = r#"
globalThis.__machineCacheIrLiveObject = { a: 1, b: 2, c: 0 };
function machineCacheIrLivePayload(target) {
  target.c = target.a + target.b;
  return target.c + target.a;
}
let livePayloadWarm = "";
for (let warm = 0; warm < 4010; warm++) {
  livePayloadWarm += "machineCacheIrLivePayload(__machineCacheIrLiveObject);";
}
eval(livePayloadWarm);
"#;

const LIVE_PAYLOAD_SETTLE: &str = r#"
for (let probeWarm = 0; probeWarm < 1000; probeWarm++) {
  machineCacheIrLivePayload(__machineCacheIrLiveObject);
}
"#;

const LIVE_PAYLOAD_PROBE: &str = r#"
JSON.stringify([
  __machineCacheIrLiveObject.c,
  machineCacheIrLivePayload(__machineCacheIrLiveObject)
]);
"#;

const WARMED_TRANSITION_SETUP: &str = r#"
function machineSnapshotAddNull(target, value) {
  target.added = value;
  return value;
}
function machineSnapshotAddDefault(target, value) {
  target.added = value;
  return value;
}
globalThis.__snapshotWritablePrototype = Object.create(null);
__snapshotWritablePrototype.added = 0;
function machineSnapshotAddInherited(target, value) {
  target.added = value;
  return value;
}
for (let warm = 0; warm < 5000; warm++) {
  const nullTarget = Object.create(null);
  nullTarget.anchor = warm;
  machineSnapshotAddNull(nullTarget, warm);
  const defaultTarget = { anchor: warm };
  machineSnapshotAddDefault(defaultTarget, warm);
  const inheritedTarget = Object.create(__snapshotWritablePrototype);
  inheritedTarget.anchor = warm;
  machineSnapshotAddInherited(inheritedTarget, warm);
}
"#;

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
    return error.message;
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
    let builder = Runtime::builder().jit_selection(JitSelection::ProductionTiered);
    if artifacts {
        builder
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .build()
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
        ("machinePropertyLoadCold", 2usize),
        ("machinePropertyStoreCold", 2usize),
    ] {
        let matching = regions
            .iter()
            .filter(|region| region["kind"] == kind)
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            expected,
            "mixed function must retain both committed {kind} regions: {code_map}"
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
        (
            u64::from(otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY.id),
            "jit_load_property_value",
            "reentrantNamedLoad",
        ),
        (
            u64::from(otter_vm::native_abi::STUB_JIT_STORE_PROPERTY.id),
            "jit_store_property_value",
            "reentrantNamedStore",
        ),
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
                relocation["target"]["kind"] == "propertySourceCell"
                    && relocation["target"]["access"] == access
            }),
            "mixed Machine body must own a stable {access} source cell: {relocations:?}"
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
fn cache_ir_boolean_plumbing_preserves_live_tagged_payloads() {
    let mut runtime = runtime(false);
    runtime
        .run_script(
            SourceInput::from_javascript(LIVE_PAYLOAD_WARM),
            "jit-machine-cache-ir-live-payload-warm.js",
        )
        .expect("warm live-payload Machine body");
    runtime
        .run_script(
            SourceInput::from_javascript(LIVE_PAYLOAD_SETTLE),
            "jit-machine-cache-ir-live-payload-settle.js",
        )
        .expect("settle live-payload Machine generation");
    let (result, delta) = run_with_delta(
        &mut runtime,
        LIVE_PAYLOAD_PROBE,
        "jit-machine-cache-ir-live-payload-probe.js",
    );
    assert_eq!(result, "[3,4]");
    assert!(
        delta.optimized_entries > 0,
        "the live-payload regression must execute Machine code: {delta:?}"
    );
    assert_eq!(
        delta.optimized_deopts, 0,
        "Boolean SSA helpers must not clobber a tagged load result: {delta:?}"
    );
}

#[test]
fn post_compile_property_misses_remain_committed_without_learning_proof_data() {
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
    assert_machine_entry_without_deopt(hot_delta, "snapshot hot-only property path");
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
    assert_exact_runtime_pair(
        reuse_delta,
        "same-shape post-compile miss without semantic self-patching",
    );

    let (frozen, frozen_delta) = run_with_delta(
        &mut runtime,
        NON_EXTENSIBLE_PROBE,
        "jit-machine-generic-properties-non-extensible.js",
    );
    assert_eq!(frozen, r#"[true,9,false,3,"anchor",false,5004]"#);
    assert_exact_runtime_pair(frozen_delta, "non-extensible add rejection");

    let (overflow_first, overflow_first_delta) = run_with_delta(
        &mut runtime,
        OUT_OF_LINE_FIRST,
        "jit-machine-generic-properties-overflow-first.js",
    );
    assert_eq!(
        overflow_first,
        r#"[true,10,true,1,2,3,44,"first,second,third,added",5005]"#
    );
    assert_exact_runtime_pair(
        overflow_first_delta,
        "inline-capacity overflow property pair",
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
    assert_exact_runtime_pair(
        overflow_reuse_delta,
        "repeated inline-capacity overflow property pair",
    );

    let (existing, existing_delta) = run_with_delta(
        &mut runtime,
        EXISTING_STORE_FILL,
        "jit-machine-generic-properties-existing-store-fill.js",
    );
    assert_eq!(existing, "[true,12,true,46,5007]");
    assert_exact_runtime_pair(existing_delta, "existing writable property pair");

    let (readonly, readonly_delta) = run_with_delta(
        &mut runtime,
        NON_WRITABLE_PROBE,
        "jit-machine-generic-properties-non-writable.js",
    );
    assert_eq!(readonly, "[true,13,true,90,false,5008]");
    assert_exact_runtime_pair(readonly_delta, "non-writable descriptor property pair");

    let (invalidated_load, invalidated_load_delta) = run_with_delta(
        &mut runtime,
        ACCESSOR_INVALIDATION_PROBE,
        "jit-machine-generic-properties-accessor-invalidation.js",
    );
    assert_eq!(invalidated_load, "[true,15,true,48,1,5009]");
    assert_exact_runtime_pair(
        invalidated_load_delta,
        "own-data to accessor invalidation property pair",
    );
}

#[test]
fn post_compile_default_prototype_adds_stay_on_the_committed_boundary() {
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
        "the first call needs one load plus one canonical default-prototype store: \
         {first_delta:?}"
    );
    assert_eq!(first_delta.reentrant_stub_transitions, 2, "{first_delta:?}");

    let (reuse, reuse_delta) = run_with_delta(
        &mut runtime,
        DEFAULT_PROTOTYPE_REUSE,
        "jit-machine-generic-properties-default-reuse.js",
    );
    assert_eq!(reuse, r#"[true,17,true,50,"anchor,added",5002]"#);
    assert_exact_runtime_pair(
        reuse_delta,
        "second post-compile default-prototype property pair",
    );
}

#[test]
fn warmed_add_transitions_execute_from_immutable_cache_ir_without_reentry() {
    let mut runtime = runtime(false);
    completion(
        &mut runtime,
        WARMED_TRANSITION_SETUP,
        "jit-machine-cache-ir-transition-setup.js",
    );
    let (null_result, null_delta) = run_with_delta(
        &mut runtime,
        r#"
globalThis.__snapshotNull = Object.create(null);
__snapshotNull.anchor = 1;
const nullResult = machineSnapshotAddNull(__snapshotNull, 41);
JSON.stringify([nullResult, __snapshotNull.added, Object.keys(__snapshotNull).join(",")]);
"#,
        "jit-machine-cache-ir-null-transition-probe.js",
    );
    assert_eq!(null_result, r#"[41,41,"anchor,added"]"#);
    assert_machine_entry_without_deopt(null_delta, "immutable null-prototype transition");
    assert_eq!(
        null_delta.runtime_property_stubs, 0,
        "the warmed null-prototype transition must remain generated: {null_delta:?}"
    );
    assert_eq!(
        null_delta.reentrant_stub_transitions, 0,
        "generated null-prototype publication must not reenter: {null_delta:?}"
    );

    let (default_result, default_delta) = run_with_delta(
        &mut runtime,
        r#"
globalThis.__snapshotDefault = { anchor: 2 };
const defaultResult = machineSnapshotAddDefault(__snapshotDefault, 42);
JSON.stringify([defaultResult, __snapshotDefault.added, Object.keys(__snapshotDefault).join(",")]);
"#,
        "jit-machine-cache-ir-default-transition-probe.js",
    );
    assert_eq!(default_result, r#"[42,42,"anchor,added"]"#);
    assert_machine_entry_without_deopt(default_delta, "immutable default-prototype transition");
    assert_eq!(
        default_delta.runtime_property_stubs, 1,
        "dictionary-backed Object.prototype must reject the whole program: {default_delta:?}"
    );
    assert_eq!(
        default_delta.reentrant_stub_transitions, 1,
        "the unsupported program must commit once through the canonical boundary: {default_delta:?}"
    );

    let (inherited_result, inherited_delta) = run_with_delta(
        &mut runtime,
        r#"
globalThis.__snapshotInherited = Object.create(__snapshotWritablePrototype);
__snapshotInherited.anchor = 3;
const inheritedResult = machineSnapshotAddInherited(__snapshotInherited, 43);
JSON.stringify([inheritedResult, __snapshotInherited.added, Object.keys(__snapshotInherited).join(",")]);
"#,
        "jit-machine-cache-ir-inherited-transition-probe.js",
    );
    assert_eq!(inherited_result, r#"[43,43,"anchor,added"]"#);
    assert_machine_entry_without_deopt(inherited_delta, "immutable inherited-writable transition");
    assert_eq!(
        inherited_delta.runtime_property_stubs, 0,
        "the warmed inherited-writable transition must remain generated: {inherited_delta:?}"
    );
    assert_eq!(
        inherited_delta.reentrant_stub_transitions, 0,
        "generated inherited-writable publication must not reenter: {inherited_delta:?}"
    );
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
fn generic_named_load_inside_local_catch_uses_explicit_machine_status() {
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
        function_bundles.iter().any(|bundle| bundle
            .file(JitArtifactFileName::OptimizedIr)
            .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))),
        "local catch must use the Machine property cold/status CFG: {:?}",
        setup.jit_debug_report()
    );
    drop(setup);

    let (caught, delta) = run_with_delta(
        &mut runtime,
        CATCH_PROBE,
        "jit-machine-generic-properties-catch-probe.js",
    );
    assert_eq!(caught, r#"["named-getter",1]"#);
    assert!(
        delta.optimized_entries > 0,
        "throwing getter must execute the Machine body: {delta:?}"
    );
    assert_eq!(
        delta.optimized_deopts, 0,
        "property throw must remain in the Machine catch CFG: {delta:?}"
    );
    assert_eq!(
        delta.runtime_property_stubs, 2,
        "target.value and catch error.message each complete once: {delta:?}"
    );
}
