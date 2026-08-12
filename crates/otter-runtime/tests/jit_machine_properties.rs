//! Machine IR named-property execution, deoptimization, and GC coverage.
//!
//! # Contents
//! - Monomorphic numeric read-modify-write and a two-shape polymorphic
//!   load/store program, including a generated Boolean existing-slot store.
//! - One `.length` site shared by dense arrays, primitive strings, and an
//!   ordinary settled own-data object, including a rope length beyond i32.
//! - Accessor, unprofiled-branch, and oversized-length misses through the fixed
//!   reentrant named-property boundary without source-operation replay.
//! - An old-parent to young-child generated store followed by full collection
//!   and reuse of the same optimized body.
//!
//! # Invariants
//! - Every hot function publishes through the scalar Machine IR backend and
//!   attributes every property region to its source bytecode PC.
//! - Every Machine property site owns a stable code-object WhiskerIC cell after
//!   its immutable snapshot program. A miss completes canonically and may fill
//!   that cell without invalidating the generated body.
//! - Accessors fire once, and already committed earlier property effects are
//!   never replayed through an exact-deopt transition.
//! - Generated cell stores preserve the collector barrier contract across
//!   moving stress collection and later full-GC reuse.
//!
//! # See also
//! - `crates/otter-jit/src/machine/numeric` owns property HIR, selection, and
//!   AArch64 emission.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactBatch, JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    SourceInput,
};

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";

const MONOMORPHIC_SETUP: &str = r#"
function makeMachinePropertyRecord(value) {
  return { value };
}

function machinePropertyRmw(record, scale, bias) {
  let total = 0;
  for (let round = 0; round < 4; round = round + 1) {
    const next = record.value * scale + bias;
    record.value = next;
    total = total + next;
  }
  return total;
}

globalThis.__machinePropertyWarmRecord = makeMachinePropertyRecord(1);
for (let warm = 0; warm < 5000; warm++) {
  machinePropertyRmw(__machinePropertyWarmRecord, 1, 0);
}
"#;

const MONOMORPHIC_FINAL: &str = r#"
globalThis.__machinePropertyFinalRecord = makeMachinePropertyRecord(1);
globalThis.__machinePropertyFinalTotal = machinePropertyRmw(
  __machinePropertyFinalRecord,
  2,
  1
);
JSON.stringify({
  total: __machinePropertyFinalTotal,
  value: __machinePropertyFinalRecord.value
});
"#;

const POLYMORPHIC_SETUP: &str = r#"
function makeMachinePropertyShapeA(value) {
  return { value };
}

function makeMachinePropertyShapeB(value) {
  return { padding: 0, value };
}

function machinePolymorphicProperty(record, value) {
  const previous = record.value;
  record.value = value;
  return previous;
}

globalThis.__machinePropertyShapeA = makeMachinePropertyShapeA(1);
globalThis.__machinePropertyShapeB = makeMachinePropertyShapeB(2);
for (let warm = 0; warm < 5000; warm++) {
  machinePolymorphicProperty(__machinePropertyShapeA, warm);
  machinePolymorphicProperty(__machinePropertyShapeB, (warm & 1) === 0);
}
"#;

const POLYMORPHIC_FINAL: &str = r#"
globalThis.__machinePropertyFinalA = makeMachinePropertyShapeA(10);
globalThis.__machinePropertyFinalB = makeMachinePropertyShapeB(20);
globalThis.__machinePropertyPreviousA = machinePolymorphicProperty(
  __machinePropertyFinalA,
  30
);
globalThis.__machinePropertyPreviousB = machinePolymorphicProperty(
  __machinePropertyFinalB,
  40
);
globalThis.__machinePropertyBeforeBoolean = machinePolymorphicProperty(
  __machinePropertyFinalA,
  true
);
JSON.stringify([
  __machinePropertyPreviousA,
  __machinePropertyPreviousB,
  __machinePropertyBeforeBoolean,
  __machinePropertyFinalA.value,
  __machinePropertyFinalB.value
]);
"#;

const ACCESSOR_SETUP: &str = r#"
function makeMachineAccessorWarmRecord(value) {
  return { value };
}

function machineAccessorProperty(record, value) {
  record.value = value;
  return record.value;
}

globalThis.__machineAccessorWarmRecord = makeMachineAccessorWarmRecord(0);
for (let warm = 0; warm < 5000; warm++) {
  machineAccessorProperty(__machineAccessorWarmRecord, warm);
}
"#;

const ACCESSOR_FINAL: &str = r#"
globalThis.__machineAccessorStored = 0;
globalThis.__machineAccessorSetterCalls = 0;
globalThis.__machineAccessorGetterCalls = 0;
globalThis.__machineAccessorReceiver = {};
Object.defineProperty(__machineAccessorReceiver, "value", {
  configurable: true,
  get() {
    __machineAccessorGetterCalls++;
    return __machineAccessorStored;
  },
  set(value) {
    __machineAccessorSetterCalls++;
    __machineAccessorStored = value;
  }
});
globalThis.__machineAccessorResult = machineAccessorProperty(
  __machineAccessorReceiver,
  41
);
JSON.stringify([
  __machineAccessorResult,
  __machineAccessorSetterCalls,
  __machineAccessorGetterCalls,
  __machineAccessorStored
]);
"#;

const COLD_BRANCH_SETUP: &str = r#"
function makeMachineColdPropertyRecord(hot) {
  return { hot, cold: 0 };
}

function machineColdPropertyBranch(record, takeCold, value) {
  const next = record.hot + 1;
  record.hot = next;
  if (takeCold) {
    record.cold = value;
    return record.cold;
  }
  return next;
}

globalThis.__machineColdPropertyWarm = makeMachineColdPropertyRecord(0);
for (let warm = 0; warm < 5000; warm++) {
  machineColdPropertyBranch(__machineColdPropertyWarm, false, 0);
}
"#;

const COLD_BRANCH_FINAL: &str = r#"
globalThis.__machineColdPropertyFinal = makeMachineColdPropertyRecord(5);
globalThis.__machineColdPropertyResult = machineColdPropertyBranch(
  __machineColdPropertyFinal,
  true,
  42
);
JSON.stringify([
  __machineColdPropertyResult,
  __machineColdPropertyFinal.hot,
  __machineColdPropertyFinal.cold
]);
"#;

const LENGTH_SETUP: &str = r#"
function makeMachineLengthRecord(length) {
  return { length };
}

function machinePropertyLength(value) {
  return value.length;
}

globalThis.__machineLengthWarmArray = [1, 2, 3];
globalThis.__machineLengthWarmString = "warm-string";
globalThis.__machineLengthWarmRecord = makeMachineLengthRecord(9);
for (let warm = 0; warm < 5000; warm++) {
  machinePropertyLength(__machineLengthWarmArray);
  machinePropertyLength(__machineLengthWarmString);
  machinePropertyLength(__machineLengthWarmRecord);
}
"#;

const LENGTH_FINAL: &str = r#"
globalThis.__machineLengthFinalArray = [1, 2, 3, 4];
globalThis.__machineLengthFinalString = "otter";
globalThis.__machineLengthFinalRecord = makeMachineLengthRecord(7);
JSON.stringify([
  machinePropertyLength(__machineLengthFinalArray),
  machinePropertyLength(__machineLengthFinalString),
  machinePropertyLength(__machineLengthFinalRecord)
]);
"#;

const LONG_STRING_LENGTH_FINAL: &str = r#"
let __machineLongString = "a";
for (let power = 0; power < 31; power++) {
  __machineLongString = __machineLongString + __machineLongString;
}
JSON.stringify([
  machinePropertyLength(__machineLongString)
]);
"#;

const BARRIER_SETUP: &str = r#"
function makeMachineBarrierParent(value) {
  return { value };
}

function machinePropertyBarrier(parent, child) {
  parent.value = child;
  return parent.value;
}

globalThis.__machineBarrierWarmParent = makeMachineBarrierParent(null);
globalThis.__machineBarrierWarmChild = { label: "warm", marker: 0 };
globalThis.__machineBarrierOldParent = makeMachineBarrierParent(null);
for (let warm = 0; warm < 5000; warm++) {
  machinePropertyBarrier(
    __machineBarrierWarmParent,
    __machineBarrierWarmChild
  );
}
"#;

const BARRIER_PROBE: &str = r#"
(function storeYoungChildIntoOldParent() {
  const child = { label: "young", marker: 41 };
  globalThis.__machineBarrierSame =
    machinePropertyBarrier(__machineBarrierOldParent, child) === child;
})();

let __machineBarrierChurn = 0;
for (let index = 0; index < 64; index++) {
  const garbage = { index, padding: "barrier-padding-" + index };
  __machineBarrierChurn += garbage.index;
}
JSON.stringify([
  __machineBarrierSame,
  __machineBarrierOldParent.value.label,
  __machineBarrierOldParent.value.marker,
  __machineBarrierChurn
]);
"#;

const BARRIER_REUSE: &str = r#"
globalThis.__machineBarrierRetained = __machineBarrierOldParent.value;
globalThis.__machineBarrierNext = { label: "again", marker: 42 };
globalThis.__machineBarrierReturned = machinePropertyBarrier(
  __machineBarrierOldParent,
  __machineBarrierNext
);
JSON.stringify([
  __machineBarrierRetained.label,
  __machineBarrierRetained.marker,
  __machineBarrierReturned === __machineBarrierNext,
  __machineBarrierOldParent.value.label,
  __machineBarrierOldParent.value.marker
]);
"#;

#[derive(Debug)]
struct FinalRun {
    completion: String,
    optimized_entries: u64,
    optimized_deopts: u64,
    runtime_property_stubs: u64,
    reentrant_stub_transitions: u64,
}

#[derive(Debug)]
struct BarrierRun {
    probe: FinalRun,
    reuse: FinalRun,
}

fn runtime(selection: JitSelection) -> Runtime {
    let builder = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX);
    if matches!(selection, JitSelection::ProductionTiered) {
        builder.jit_debug(JitDebugRequest::artifacts()).build()
    } else {
        builder.build()
    }
    .expect("Machine property runtime")
}

fn assert_machine_property_artifact(
    artifacts: &JitArtifactBatch,
    module: &str,
    function_name: &str,
    minimum_loads: usize,
    minimum_stores: usize,
) {
    let bundle = artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == module
                && manifest.function_name() == function_name
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
            panic!(
                "missing exact Machine IR property bundle for {module}:{function_name}; \
                 bundles={manifests:?}"
            );
        });

    let code_map: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::CodeMap)
            .expect("Machine property code map")
            .contents(),
    )
    .expect("valid Machine property code-map JSON");
    let regions = code_map["regions"]
        .as_array()
        .expect("Machine property code-map regions");
    assert!(
        regions
            .iter()
            .any(|region| region["kind"] == "machineScalarFunction"),
        "{function_name} must expose its Machine scalar body: {code_map}"
    );
    for (kind, minimum) in [
        ("machinePropertyLoad", minimum_loads),
        ("machinePropertyStore", minimum_stores),
    ] {
        let matching = regions
            .iter()
            .filter(|region| region["kind"] == kind)
            .collect::<Vec<_>>();
        assert!(
            matching.len() >= minimum,
            "{function_name} must expose at least {minimum} {kind} regions: {code_map}"
        );
        assert!(
            matching
                .iter()
                .all(|region| region["bytePc"].as_u64().is_some()),
            "{function_name} must attribute every {kind} to bytecode: {code_map}"
        );
    }

    let relocations: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::Relocations)
            .expect("Machine property relocations")
            .contents(),
    )
    .expect("valid Machine property relocation JSON");
    let relocations = relocations["relocations"]
        .as_array()
        .expect("Machine property relocation entries");
    for (access, minimum) in [("load", minimum_loads), ("store", minimum_stores)] {
        let matching = relocations
            .iter()
            .filter(|relocation| {
                relocation["target"]["kind"] == "propertyIcCell"
                    && relocation["target"]["access"] == access
            })
            .count();
        assert!(
            matching >= minimum,
            "{function_name} must own at least {minimum} {access} IC-cell relocations: \
             {relocations:?}"
        );
    }
    for (minimum, stub_id, stub, signature) in [
        (
            minimum_loads,
            19u64,
            "jit_load_property_value",
            "reentrantNamedLoad",
        ),
        (
            minimum_stores,
            20u64,
            "jit_store_property_value",
            "reentrantNamedStore",
        ),
    ] {
        if minimum > 0 {
            assert!(
                relocations.iter().any(|relocation| {
                    relocation["target"]["kind"] == "runtimeStub"
                        && relocation["target"]["id"].as_u64() == Some(stub_id)
                        && relocation["target"]["name"] == stub
                        && relocation["target"]["signature"] == signature
                }),
                "{function_name} must retain fixed stub {stub_id}:{stub}:{signature}: \
                 {relocations:?}"
            );
        }
    }

    let safepoints: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::Safepoints)
            .expect("Machine property safepoints")
            .contents(),
    )
    .expect("valid Machine property safepoint JSON");
    assert!(
        safepoints["safepoints"]
            .as_array()
            .is_some_and(|safepoints| safepoints.len() >= minimum_loads + minimum_stores),
        "{function_name} must publish roots for every miss-capable property site: {safepoints}"
    );
}

#[allow(clippy::too_many_arguments)]
fn run_fixture(
    selection: JitSelection,
    setup: &'static str,
    setup_module: &'static str,
    function_name: &'static str,
    minimum_loads: usize,
    minimum_stores: usize,
    final_source: &'static str,
    final_module: &'static str,
) -> FinalRun {
    let mut runtime = runtime(selection);
    let setup_result = runtime
        .run_script(SourceInput::from_javascript(setup), setup_module)
        .unwrap_or_else(|error| panic!("Machine property setup {setup_module}: {error:?}"));
    if matches!(selection, JitSelection::ProductionTiered) {
        assert_machine_property_artifact(
            setup_result
                .jit_artifacts()
                .expect("enabled Machine property artifact batch"),
            setup_module,
            function_name,
            minimum_loads,
            minimum_stores,
        );
    }
    drop(setup_result);

    let before = runtime.execution_stats();
    let completion = runtime
        .run_script(SourceInput::from_javascript(final_source), final_module)
        .unwrap_or_else(|error| panic!("Machine property final call {final_module}: {error:?}"))
        .completion_string()
        .to_owned();
    let after = runtime.execution_stats();
    FinalRun {
        completion,
        optimized_entries: after.jit_optimized_entries - before.jit_optimized_entries,
        optimized_deopts: after.jit_optimized_deopts - before.jit_optimized_deopts,
        runtime_property_stubs: after.jit_runtime_property_stubs
            - before.jit_runtime_property_stubs,
        reentrant_stub_transitions: after.jit_reentrant_stub_transitions
            - before.jit_reentrant_stub_transitions,
    }
}

fn run_barrier_fixture(selection: JitSelection) -> BarrierRun {
    let mut runtime = runtime(selection);
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(BARRIER_SETUP),
            "jit-machine-properties-barrier-setup.js",
        )
        .expect("Machine property barrier setup");
    if matches!(selection, JitSelection::ProductionTiered) {
        assert_machine_property_artifact(
            setup
                .jit_artifacts()
                .expect("enabled Machine property barrier artifacts"),
            "jit-machine-properties-barrier-setup.js",
            "machinePropertyBarrier",
            1,
            1,
        );
    }
    drop(setup);

    runtime
        .force_gc()
        .expect("first full GC ages the barrier parent");
    runtime
        .force_gc()
        .expect("second full GC leaves the barrier parent old and stable");

    let before_probe = runtime.execution_stats();
    let probe_completion = runtime
        .run_script(
            SourceInput::from_javascript(BARRIER_PROBE),
            "jit-machine-properties-barrier-probe.js",
        )
        .expect("generated old-to-young property store")
        .completion_string()
        .to_owned();
    let after_probe = runtime.execution_stats();
    let probe = FinalRun {
        completion: probe_completion,
        optimized_entries: after_probe.jit_optimized_entries - before_probe.jit_optimized_entries,
        optimized_deopts: after_probe.jit_optimized_deopts - before_probe.jit_optimized_deopts,
        runtime_property_stubs: after_probe.jit_runtime_property_stubs
            - before_probe.jit_runtime_property_stubs,
        reentrant_stub_transitions: after_probe.jit_reentrant_stub_transitions
            - before_probe.jit_reentrant_stub_transitions,
    };

    runtime
        .force_gc()
        .expect("full GC follows the generated old-to-young store");
    let before_reuse = runtime.execution_stats();
    let reuse_completion = runtime
        .run_script(
            SourceInput::from_javascript(BARRIER_REUSE),
            "jit-machine-properties-barrier-reuse.js",
        )
        .expect("reuse generated property body after full GC")
        .completion_string()
        .to_owned();
    let after_reuse = runtime.execution_stats();
    let reuse = FinalRun {
        completion: reuse_completion,
        optimized_entries: after_reuse.jit_optimized_entries - before_reuse.jit_optimized_entries,
        optimized_deopts: after_reuse.jit_optimized_deopts - before_reuse.jit_optimized_deopts,
        runtime_property_stubs: after_reuse.jit_runtime_property_stubs
            - before_reuse.jit_runtime_property_stubs,
        reentrant_stub_transitions: after_reuse.jit_reentrant_stub_transitions
            - before_reuse.jit_reentrant_stub_transitions,
    };

    BarrierRun { probe, reuse }
}

#[test]
fn monomorphic_numeric_rmw_uses_machine_properties_without_deopt() {
    let oracle = run_fixture(
        JitSelection::InterpreterOnly,
        MONOMORPHIC_SETUP,
        "jit-machine-properties-rmw-setup.js",
        "machinePropertyRmw",
        1,
        1,
        MONOMORPHIC_FINAL,
        "jit-machine-properties-rmw-final.js",
    );
    let compiled = run_fixture(
        JitSelection::ProductionTiered,
        MONOMORPHIC_SETUP,
        "jit-machine-properties-rmw-setup.js",
        "machinePropertyRmw",
        1,
        1,
        MONOMORPHIC_FINAL,
        "jit-machine-properties-rmw-final.js",
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, r#"{"total":56,"value":31}"#);
    assert!(
        compiled.optimized_entries > 0,
        "final property RMW must enter optimized Machine IR: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 0,
        "settled monomorphic property RMW must stay generated: {compiled:?}"
    );
    assert_eq!(
        (
            compiled.runtime_property_stubs,
            compiled.reentrant_stub_transitions
        ),
        (0, 0),
        "the settled snapshot program must not enter its cold boundary: {compiled:?}"
    );
}

#[test]
fn two_shape_load_store_and_boolean_existing_slot_stay_generated() {
    let oracle = run_fixture(
        JitSelection::InterpreterOnly,
        POLYMORPHIC_SETUP,
        "jit-machine-properties-polymorphic-setup.js",
        "machinePolymorphicProperty",
        1,
        1,
        POLYMORPHIC_FINAL,
        "jit-machine-properties-polymorphic-final.js",
    );
    let compiled = run_fixture(
        JitSelection::ProductionTiered,
        POLYMORPHIC_SETUP,
        "jit-machine-properties-polymorphic-setup.js",
        "machinePolymorphicProperty",
        1,
        1,
        POLYMORPHIC_FINAL,
        "jit-machine-properties-polymorphic-final.js",
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[10,20,30,true,40]");
    assert!(
        compiled.optimized_entries >= 3,
        "both shapes and the Boolean store must enter Machine IR: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 0,
        "both settled shapes and the Boolean existing slot must hit: {compiled:?}"
    );
    assert_eq!(
        (
            compiled.runtime_property_stubs,
            compiled.reentrant_stub_transitions
        ),
        (0, 0),
        "both prepared shapes must bypass their empty dynamic cells: {compiled:?}"
    );
}

#[test]
fn accessor_miss_completes_in_place_and_runs_getter_and_setter_once() {
    let oracle = run_fixture(
        JitSelection::InterpreterOnly,
        ACCESSOR_SETUP,
        "jit-machine-properties-accessor-setup.js",
        "machineAccessorProperty",
        1,
        1,
        ACCESSOR_FINAL,
        "jit-machine-properties-accessor-final.js",
    );
    let compiled = run_fixture(
        JitSelection::ProductionTiered,
        ACCESSOR_SETUP,
        "jit-machine-properties-accessor-setup.js",
        "machineAccessorProperty",
        1,
        1,
        ACCESSOR_FINAL,
        "jit-machine-properties-accessor-final.js",
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[41,1,1,41]");
    assert!(
        compiled.optimized_entries > 0,
        "accessor receiver must reach the generated property guard: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 0,
        "accessor effects must complete through the fixed boundary without replay: {compiled:?}"
    );
    assert_eq!(
        compiled.runtime_property_stubs, 2,
        "the accessor store and load must each execute one canonical boundary: {compiled:?}"
    );
    assert_eq!(
        compiled.reentrant_stub_transitions, 2,
        "the setter and getter boundaries must each reenter exactly once: {compiled:?}"
    );
}

#[test]
fn never_taken_unprofiled_property_branch_completes_once_without_deopt() {
    let oracle = run_fixture(
        JitSelection::InterpreterOnly,
        COLD_BRANCH_SETUP,
        "jit-machine-properties-cold-setup.js",
        "machineColdPropertyBranch",
        2,
        2,
        COLD_BRANCH_FINAL,
        "jit-machine-properties-cold-final.js",
    );
    let compiled = run_fixture(
        JitSelection::ProductionTiered,
        COLD_BRANCH_SETUP,
        "jit-machine-properties-cold-setup.js",
        "machineColdPropertyBranch",
        2,
        2,
        COLD_BRANCH_FINAL,
        "jit-machine-properties-cold-final.js",
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[42,6,42]");
    assert!(
        compiled.optimized_entries > 0,
        "the cold branch must start in the complete Machine body: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 0,
        "the first unprofiled cold property pair must not replay through deopt: {compiled:?}"
    );
    assert_eq!(
        compiled.runtime_property_stubs, 2,
        "the cold existing-slot store and load must each complete once: {compiled:?}"
    );
    assert_eq!(
        compiled.reentrant_stub_transitions, 2,
        "both cold property operations must cross the fixed boundary once: {compiled:?}"
    );
}

#[test]
fn array_string_and_ordinary_object_length_share_one_machine_site() {
    let oracle = run_fixture(
        JitSelection::InterpreterOnly,
        LENGTH_SETUP,
        "jit-machine-properties-length-setup.js",
        "machinePropertyLength",
        1,
        0,
        LENGTH_FINAL,
        "jit-machine-properties-length-final.js",
    );
    let compiled = run_fixture(
        JitSelection::ProductionTiered,
        LENGTH_SETUP,
        "jit-machine-properties-length-setup.js",
        "machinePropertyLength",
        1,
        0,
        LENGTH_FINAL,
        "jit-machine-properties-length-final.js",
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[4,5,7]");
    assert!(
        compiled.optimized_entries >= 3,
        "array, string, and ordinary object must enter one Machine site: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 0,
        "all three .length receiver classes must stay generated: {compiled:?}"
    );
    assert_eq!(
        (
            compiled.runtime_property_stubs,
            compiled.reentrant_stub_transitions
        ),
        (0, 0),
        "all prepared .length classes must bypass the cold boundary: {compiled:?}"
    );
}

#[test]
fn string_length_beyond_int32_completes_in_place_without_wrapping() {
    let oracle = run_fixture(
        JitSelection::InterpreterOnly,
        LENGTH_SETUP,
        "jit-machine-properties-long-string-setup.js",
        "machinePropertyLength",
        1,
        0,
        LONG_STRING_LENGTH_FINAL,
        "jit-machine-properties-long-string-final.js",
    );
    let compiled = run_fixture(
        JitSelection::ProductionTiered,
        LENGTH_SETUP,
        "jit-machine-properties-long-string-setup.js",
        "machinePropertyLength",
        1,
        0,
        LONG_STRING_LENGTH_FINAL,
        "jit-machine-properties-long-string-final.js",
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[2147483648]");
    assert!(
        compiled.optimized_entries > 0,
        "the oversized rope must reach the Machine length guard: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 0,
        "the oversized rope must use canonical Number boxing without replay: {compiled:?}"
    );
    assert_eq!(
        compiled.runtime_property_stubs, 1,
        "the oversized rope must enter the named-load boundary once: {compiled:?}"
    );
    assert_eq!(
        compiled.reentrant_stub_transitions, 1,
        "the oversized rope load must cross one reentrant boundary: {compiled:?}"
    );
}

#[test]
fn old_parent_young_child_store_survives_full_gc_and_machine_reuse() {
    let oracle = run_barrier_fixture(JitSelection::InterpreterOnly);
    let compiled = run_barrier_fixture(JitSelection::ProductionTiered);

    assert_eq!(compiled.probe.completion, oracle.probe.completion);
    assert_eq!(compiled.probe.completion, r#"[true,"young",41,2016]"#);
    assert!(
        compiled.probe.optimized_entries > 0,
        "old-to-young store must enter optimized Machine IR: {compiled:?}"
    );
    assert_eq!(
        compiled.probe.optimized_deopts, 0,
        "settled cell store and write barrier must not deopt: {compiled:?}"
    );
    assert_eq!(
        (
            compiled.probe.runtime_property_stubs,
            compiled.probe.reentrant_stub_transitions
        ),
        (0, 0),
        "the settled old-to-young store must bypass the miss boundary: {compiled:?}"
    );

    assert_eq!(compiled.reuse.completion, oracle.reuse.completion);
    assert_eq!(compiled.reuse.completion, r#"["young",41,true,"again",42]"#);
    assert!(
        compiled.reuse.optimized_entries > 0,
        "the same Machine property body must remain reusable after full GC: {compiled:?}"
    );
    assert_eq!(
        compiled.reuse.optimized_deopts, 0,
        "post-GC shape/slot guards must remain valid: {compiled:?}"
    );
    assert_eq!(
        (
            compiled.reuse.runtime_property_stubs,
            compiled.reuse.reentrant_stub_transitions
        ),
        (0, 0),
        "post-GC reuse must stay on the settled program: {compiled:?}"
    );
}
