//! Machine IR indexed-element execution and deoptimization coverage.
//!
//! # Contents
//! - Dense numeric read-modify-write parity with exact Machine IR artifact
//!   attribution for both generated element operations.
//! - A packed-to-tagged receiver transition proving exact resume without
//!   replaying an effect, followed by clean reuse of the runtime.
//! - A delete-driven hole transition whose canonical read observes an inherited
//!   index, advances feedback, and replaces the stale packed generation once.
//! - Fixed-length typed-array load/store misses after resizable-buffer
//!   shrinkage, including regrowth proof that an out-of-bounds store never ran.
//! - A generated constructor with fresh capture cells between element
//!   accesses, covering allocator-root refresh, direct-call linkage state, and
//!   a direct Machine IR read of the captured constructor binding.
//!
//! # Invariants
//! - Every production fixture publishes the named hot function through the
//!   scalar Machine IR backend, with family-specific load/store regions in
//!   that exact function's code map.
//! - Generated packed-element guards deopt at the original bytecode before
//!   mutation; canonical resume executes each effect once and does not evict
//!   the reusable numeric body.
//! - A fixed typed view is wholly out of bounds when its original extent no
//!   longer fits its live backing buffer, even when the selected index remains
//!   inside the buffer's retained prefix.
//! - A hot captured-binding read uses a bytecode-attributed Machine region and
//!   never retains the generic `jit_load_upvalue_value` runtime transition.
//!
//! # See also
//! - `crates/otter-jit/src/machine/numeric` owns element HIR, selection, and
//!   AArch64 emission.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTier,
    JitSelection, Runtime, SourceInput,
};

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";

const DENSE_RMW_SETUP: &str = r#"
function machineDenseRmw(values, scale, bias) {
  let total = 0;
  for (let index = 0; index < 4; index = index + 1) {
    const next = values[index] * scale + bias;
    values[index] = next;
    total = total + next;
  }
  return total;
}

globalThis.__machineDenseWarm = [1, 2, 3, 4];
for (let warm = 0; warm < 5000; warm++) {
  machineDenseRmw(__machineDenseWarm, 1, 0);
}
"#;

const DENSE_RMW_FINAL: &str = r#"
globalThis.__machineDenseFinal = [1, 2, 3, 4];
globalThis.__machineDenseTotal = machineDenseRmw(__machineDenseFinal, 2, 1);
JSON.stringify({ total: __machineDenseTotal, values: __machineDenseFinal });
"#;

const NO_REPLAY_SETUP: &str = r#"
function machineElementNoReplay(values, effects) {
  const loaded = values[0];
  effects[0] = effects[0] + 1;
  return loaded * 2;
}

globalThis.__machineReplayWarmValues = [7, 0];
globalThis.__machineReplayWarmEffects = [0];
for (let warm = 0; warm < 5000; warm++) {
  machineElementNoReplay(__machineReplayWarmValues, __machineReplayWarmEffects);
}
"#;

const NO_REPLAY_FINAL: &str = r#"
__machineReplayWarmValues[0] = "3";
__machineReplayWarmEffects[0] = 0;
globalThis.__machineReplayFinalResult = machineElementNoReplay(
  __machineReplayWarmValues,
  __machineReplayWarmEffects
);
globalThis.__machineReplayReuseValues = [8, 0];
globalThis.__machineReplayReuseEffects = [0];
globalThis.__machineReplayReuseResult = machineElementNoReplay(
  __machineReplayReuseValues,
  __machineReplayReuseEffects
);
JSON.stringify([
  __machineReplayFinalResult,
  __machineReplayWarmEffects[0],
  __machineReplayReuseResult,
  __machineReplayReuseEffects[0]
]);
"#;

const HOLE_TRANSITION_SETUP: &str = r#"
function machineElementHoleTransition(values, effects) {
  const loaded = values[1];
  effects[0] = effects[0] + 1;
  return loaded;
}

globalThis.__machineHoleValues = [10, 20, 30];
globalThis.__machineHoleEffects = [0];
for (let warm = 0; warm < 5000; warm++) {
  machineElementHoleTransition(__machineHoleValues, __machineHoleEffects);
}
"#;

const HOLE_TRANSITION_FINAL: &str = r#"
Array.prototype[1] = 41;
delete __machineHoleValues[1];
__machineHoleEffects[0] = 0;
globalThis.__machineHoleFirst = machineElementHoleTransition(
  __machineHoleValues,
  __machineHoleEffects
);
globalThis.__machineHoleSecond = machineElementHoleTransition(
  __machineHoleValues,
  __machineHoleEffects
);
globalThis.__machineHoleInherited = 1 in __machineHoleValues;
globalThis.__machineHoleOwn = Object.prototype.hasOwnProperty.call(
  __machineHoleValues,
  1
);
delete Array.prototype[1];
JSON.stringify([
  __machineHoleFirst,
  __machineHoleSecond,
  __machineHoleEffects[0],
  __machineHoleInherited,
  __machineHoleOwn
]);
"#;

const FIXED_RAB_SETUP: &str = r#"
function machineFixedRabElement(view, next, write) {
  if (write) {
    view[0] = next;
    return view[0];
  }
  return view[0];
}

globalThis.__machineFixedRab = new ArrayBuffer(16, { maxByteLength: 32 });
globalThis.__machineFixedRabView = new Int32Array(__machineFixedRab, 0, 4);
__machineFixedRabView[0] = 11;
for (let warm = 0; warm < 5000; warm++) {
  machineFixedRabElement(__machineFixedRabView, 11, false);
  machineFixedRabElement(__machineFixedRabView, 11, true);
}
"#;

const FIXED_RAB_FINAL: &str = r#"
__machineFixedRab.resize(4);
globalThis.__machineFixedRabLoad = machineFixedRabElement(
  __machineFixedRabView,
  0,
  false
);
globalThis.__machineFixedRabStore = machineFixedRabElement(
  __machineFixedRabView,
  99,
  true
);
__machineFixedRab.resize(16);
JSON.stringify([
  __machineFixedRabLoad,
  __machineFixedRabStore,
  __machineFixedRabView[0]
]);
"#;

const CONSTRUCT_SETUP: &str = r#"
function makeMachineConstructFixture() {
  function MachineElementBox(value) {
    function readCapturedValue() { return value; }
    this.read = readCapturedValue;
  }

  return function machineElementsAroundConstruct(values) {
    let total = 0;
    for (let index = 0; index < 4; index = index + 1) {
      new MachineElementBox(values[index]);
      values[index] = values[index] + 1;
      total = total + values[index];
    }
    return total;
  };
}

globalThis.machineElementsAroundConstruct = makeMachineConstructFixture();
globalThis.__machineConstructWarm = [1, 2, 3, 4];
for (let warm = 0; warm < 5000; warm++) {
  machineElementsAroundConstruct(__machineConstructWarm);
}
"#;

const CONSTRUCT_FINAL: &str = r#"
globalThis.__machineConstructFinal = [1, 2, 3, 4];
globalThis.__machineConstructTotal = machineElementsAroundConstruct(
  __machineConstructFinal
);
JSON.stringify({
  total: __machineConstructTotal,
  values: __machineConstructFinal
});
"#;

#[derive(Debug)]
struct FinalRun {
    completion: String,
    optimized_entries: u64,
    optimized_deopts: u64,
    compile_attempts: u64,
    code_generations: u64,
}

#[derive(Clone, Copy)]
enum MachineArtifactShape {
    PackedDoubleElements,
    GenericElements,
    PackedDoubleElementsAroundConstructCapture,
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
    .expect("Machine element runtime")
}

fn assert_machine_element_artifact(
    artifacts: &JitArtifactBatch,
    module: &str,
    function_name: &str,
    shape: MachineArtifactShape,
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
                "missing exact Machine IR bundle for {module}:{function_name}; bundles={manifests:?}"
            );
        });
    let code_map: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::CodeMap)
            .expect("Machine element code map")
            .contents(),
    )
    .expect("valid Machine element code-map JSON");
    let regions = code_map["regions"]
        .as_array()
        .expect("Machine element code-map regions");
    let (load_kind, store_kind) = match shape {
        MachineArtifactShape::PackedDoubleElements
        | MachineArtifactShape::PackedDoubleElementsAroundConstructCapture => (
            "machinePackedDoubleElementLoad",
            "machinePackedDoubleElementStore",
        ),
        MachineArtifactShape::GenericElements => ("machineElementLoad", "machineElementStore"),
    };
    for kind in ["machineScalarFunction", load_kind, store_kind] {
        let matching = regions
            .iter()
            .filter(|region| region["kind"] == kind)
            .collect::<Vec<_>>();
        assert!(
            !matching.is_empty(),
            "{function_name} must expose {kind}: {code_map}"
        );
        if kind != "machineScalarFunction" {
            assert!(
                matching
                    .iter()
                    .all(|region| region["bytePc"].as_u64().is_some()),
                "{function_name} must attribute every {kind} to bytecode: {code_map}"
            );
        }
    }
    if matches!(
        shape,
        MachineArtifactShape::PackedDoubleElementsAroundConstructCapture
    ) {
        let upvalue_loads = regions
            .iter()
            .filter(|region| region["kind"] == "machineUpvalueLoad")
            .collect::<Vec<_>>();
        assert!(
            !upvalue_loads.is_empty()
                && upvalue_loads
                    .iter()
                    .all(|region| region["bytePc"].as_u64().is_some()),
            "{function_name} must expose bytecode-attributed direct upvalue reads: {code_map}"
        );
        assert_construct_capture_artifact(bundle, function_name);
    }
}

fn assert_construct_capture_artifact(bundle: &JitArtifactBundle, function_name: &str) {
    let relocations: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::Relocations)
            .expect("construct relocation payload")
            .contents(),
    )
    .expect("valid construct relocation JSON");
    let relocations = relocations["relocations"]
        .as_array()
        .expect("construct relocation entries");
    let direct_construct = relocations
        .iter()
        .find(|relocation| {
            relocation["target"]["kind"] == "directCallEntryCell"
                && relocation["target"]["directCall"]["callKind"] == "construct"
        })
        .unwrap_or_else(|| {
            panic!("{function_name} must retain its generated direct base construct")
        });
    assert_eq!(
        direct_construct["target"]["directCall"]["ownUpvalueCount"], 1,
        "the direct constructor must initialize its one fresh capture cell: {direct_construct}"
    );
    for runtime_stub in [
        "jit_try_prepare_base_construct",
        "jit_prepare_base_construct",
        "jit_initialize_upvalues",
    ] {
        assert!(
            relocations.iter().any(|relocation| {
                relocation["target"]["kind"] == "runtimeStub"
                    && relocation["target"]["name"] == runtime_stub
            }),
            "{function_name} must retain {runtime_stub}: {relocations:?}"
        );
    }
    assert!(
        relocations.iter().all(|relocation| {
            relocation["target"]["kind"] != "runtimeStub"
                || relocation["target"]["name"] != "jit_load_upvalue_value"
        }),
        "{function_name} must load captured bindings directly: {relocations:?}"
    );
}

fn run_fixture(
    selection: JitSelection,
    setup: &'static str,
    setup_module: &'static str,
    function_name: &'static str,
    artifact_shape: MachineArtifactShape,
    final_source: &'static str,
    final_module: &'static str,
) -> FinalRun {
    let mut runtime = runtime(selection);
    let setup_result = runtime
        .run_script(SourceInput::from_javascript(setup), setup_module)
        .unwrap_or_else(|error| panic!("Machine element setup {setup_module}: {error:?}"));
    if matches!(selection, JitSelection::ProductionTiered) {
        assert_machine_element_artifact(
            setup_result
                .jit_artifacts()
                .expect("enabled Machine element artifact batch"),
            setup_module,
            function_name,
            artifact_shape,
        );
    }
    drop(setup_result);

    let before = runtime.execution_stats();
    let completion = runtime
        .run_script(SourceInput::from_javascript(final_source), final_module)
        .unwrap_or_else(|error| panic!("Machine element final call {final_module}: {error:?}"))
        .completion_string()
        .to_owned();
    let after = runtime.execution_stats();
    FinalRun {
        completion,
        optimized_entries: after.jit_optimized_entries - before.jit_optimized_entries,
        optimized_deopts: after.jit_optimized_deopts - before.jit_optimized_deopts,
        compile_attempts: after.jit_compile_attempts - before.jit_compile_attempts,
        code_generations: after.jit_code_generations - before.jit_code_generations,
    }
}

#[test]
fn dense_numeric_rmw_uses_machine_element_regions_without_deopt() {
    let oracle = run_fixture(
        JitSelection::InterpreterOnly,
        DENSE_RMW_SETUP,
        "jit-machine-elements-rmw-setup.js",
        "machineDenseRmw",
        MachineArtifactShape::PackedDoubleElements,
        DENSE_RMW_FINAL,
        "jit-machine-elements-rmw-final.js",
    );
    let compiled = run_fixture(
        JitSelection::ProductionTiered,
        DENSE_RMW_SETUP,
        "jit-machine-elements-rmw-setup.js",
        "machineDenseRmw",
        MachineArtifactShape::PackedDoubleElements,
        DENSE_RMW_FINAL,
        "jit-machine-elements-rmw-final.js",
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, r#"{"total":24,"values":[3,5,7,9]}"#);
    assert!(
        compiled.optimized_entries > 0,
        "final dense RMW must enter optimized Machine IR: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 0,
        "supported dense RMW must remain generated: {compiled:?}"
    );
}

#[test]
fn packed_to_tagged_transition_deopts_once_without_replay_and_keeps_runtime_reusable() {
    let oracle = run_fixture(
        JitSelection::InterpreterOnly,
        NO_REPLAY_SETUP,
        "jit-machine-elements-no-replay-setup.js",
        "machineElementNoReplay",
        MachineArtifactShape::PackedDoubleElements,
        NO_REPLAY_FINAL,
        "jit-machine-elements-no-replay-final.js",
    );
    let compiled = run_fixture(
        JitSelection::ProductionTiered,
        NO_REPLAY_SETUP,
        "jit-machine-elements-no-replay-setup.js",
        "machineElementNoReplay",
        MachineArtifactShape::PackedDoubleElements,
        NO_REPLAY_FINAL,
        "jit-machine-elements-no-replay-final.js",
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[6,1,16,1]");
    assert!(
        compiled.optimized_entries > 0,
        "the tagged transition must first enter optimized Machine IR: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 1,
        "the tagged transition must deopt once, execute each effect once, and not storm on reuse: {compiled:?}"
    );
}

#[test]
fn hole_transition_reads_the_prototype_and_recompiles_without_a_deopt_storm() {
    let oracle = run_fixture(
        JitSelection::InterpreterOnly,
        HOLE_TRANSITION_SETUP,
        "jit-machine-elements-hole-setup.js",
        "machineElementHoleTransition",
        MachineArtifactShape::PackedDoubleElements,
        HOLE_TRANSITION_FINAL,
        "jit-machine-elements-hole-final.js",
    );
    let compiled = run_fixture(
        JitSelection::ProductionTiered,
        HOLE_TRANSITION_SETUP,
        "jit-machine-elements-hole-setup.js",
        "machineElementHoleTransition",
        MachineArtifactShape::PackedDoubleElements,
        HOLE_TRANSITION_FINAL,
        "jit-machine-elements-hole-final.js",
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[41,41,2,true,false]");
    assert!(
        compiled.optimized_entries > 0,
        "the first hole probe must reach the stale packed generation: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 1,
        "the hole layout transition must advance feedback after one exact miss, not deopt once per call: {compiled:?}"
    );
    assert!(
        compiled.compile_attempts > 0 && compiled.code_generations > 0,
        "the feedback epoch change must admit a replacement generation: {compiled:?}"
    );
}

#[test]
fn fixed_typed_view_shrink_deopts_load_and_store_before_effects() {
    let oracle = run_fixture(
        JitSelection::InterpreterOnly,
        FIXED_RAB_SETUP,
        "jit-machine-elements-fixed-rab-setup.js",
        "machineFixedRabElement",
        MachineArtifactShape::GenericElements,
        FIXED_RAB_FINAL,
        "jit-machine-elements-fixed-rab-final.js",
    );
    let compiled = run_fixture(
        JitSelection::ProductionTiered,
        FIXED_RAB_SETUP,
        "jit-machine-elements-fixed-rab-setup.js",
        "machineFixedRabElement",
        MachineArtifactShape::GenericElements,
        FIXED_RAB_FINAL,
        "jit-machine-elements-fixed-rab-final.js",
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[null,null,11]");
    assert!(
        compiled.optimized_entries >= 2,
        "both fixed-view misses must enter the Machine body: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 2,
        "load and store must each exact-deopt before touching the OOB fixed view: {compiled:?}"
    );
}

#[test]
fn element_roots_survive_generated_constructor_capture_initialization() {
    let oracle = run_fixture(
        JitSelection::InterpreterOnly,
        CONSTRUCT_SETUP,
        "jit-machine-elements-construct-setup.js",
        "machineElementsAroundConstruct",
        MachineArtifactShape::PackedDoubleElementsAroundConstructCapture,
        CONSTRUCT_FINAL,
        "jit-machine-elements-construct-final.js",
    );
    let compiled = run_fixture(
        JitSelection::ProductionTiered,
        CONSTRUCT_SETUP,
        "jit-machine-elements-construct-setup.js",
        "machineElementsAroundConstruct",
        MachineArtifactShape::PackedDoubleElementsAroundConstructCapture,
        CONSTRUCT_FINAL,
        "jit-machine-elements-construct-final.js",
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, r#"{"total":14,"values":[2,3,4,5]}"#);
    assert!(
        compiled.optimized_entries > 0,
        "final construct fixture must enter optimized Machine IR: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 0,
        "generated constructor linkage must preserve Machine element roots: {compiled:?}"
    );
}
