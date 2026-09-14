//! Stack-owned generated-callee `ArrayConstruct` coverage.
//!
//! # Contents
//! - Zero-argument and one-Int32-length Array construction at callee entry.
//! - Invalid-length exact deoptimization and canonical `RangeError` without replay.
//! - Moving-GC preservation of live arguments and an earlier Array result.
//! - Machine direct-call artifacts tying each caller to the exact callee body.
//! - Exact ordinary function-constructor field-transition publication.
//! - Pre-commit extensibility guards and out-of-line field-slab preservation.
//!
//! # Invariants
//! - `ArrayConstruct` is the first effectful operation in each focused callee;
//!   parameter loads may precede it.
//! - Every probe enters that callee through generated stack-owned linkage.
//! - A successful allocation returns once; pre-effect misses deopt exactly once.
//! - Completed calls unlink their native roots and leave the caller reusable.
//!
//! # See also
//! - `otter_vm::runtime_activation` for stack-owned semantic operations.
//! - `otter-jit::arm64::direct_call` for generated frame publication.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTarget,
    JitDebugTier, JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";

const NORMAL_SETUP: &str = r#"
function arrayZeroAtEntry() {
  const result = new Array();
  if (typeof result !== "object") throw "bad-array";
  return result.length;
}

function arrayLengthAtEntry(length) {
  return new Array(length).length;
}

function callArrayZero(fn) {
  return fn();
}

function callArrayLength(fn, length) {
  return fn(length);
}

for (let warm = 0; warm < 5000; warm++) {
  arrayZeroAtEntry();
  arrayLengthAtEntry(4);
  callArrayZero(arrayZeroAtEntry);
  callArrayLength(arrayLengthAtEntry, 4);
}
"#;

const NORMAL_PROBE: &str = r#"
JSON.stringify([
  callArrayZero(arrayZeroAtEntry),
  callArrayLength(arrayLengthAtEntry, 4),
  callArrayZero(arrayZeroAtEntry),
  callArrayLength(arrayLengthAtEntry, 7)
]);
"#;

const INVALID_LENGTH_SETUP: &str = r#"
globalThis.__stackArrayLengthEffects = [0];

function invalidArrayLength(length, effects) {
  effects[0] = effects[0] + 1;
  return new Array(length).length;
}

function callInvalidArrayLength(fn, length, effects) {
  return fn(length, effects);
}

for (let warm = 0; warm < 5000; warm++) {
  invalidArrayLength(4, __stackArrayLengthEffects);
  callInvalidArrayLength(invalidArrayLength, 4, __stackArrayLengthEffects);
}
globalThis.__stackArrayLengthEffects[0] = 0;
"#;

const INVALID_LENGTH_PROBE: &str = r#"
(() => {
  let name = "missing";
  let message = "missing";
  try {
    callInvalidArrayLength(invalidArrayLength, -1, __stackArrayLengthEffects);
  } catch (error) {
    name = error.name;
    message = error.message;
  }
  const attemptsAfterThrow = globalThis.__stackArrayLengthEffects[0];
  const recoveredLength = callInvalidArrayLength(
    invalidArrayLength,
    3,
    __stackArrayLengthEffects
  );
  return JSON.stringify([
    name,
    message,
    attemptsAfterThrow,
    recoveredLength,
    globalThis.__stackArrayLengthEffects[0]
  ]);
})();
"#;

const GC_SETUP: &str = r#"
function gcArrayZeroAtEntry(marker) {
  const result = new Array();
  const trigger = new Array(1);
  if (typeof result !== "object" || marker.value < 0 || trigger.length !== 1) {
    throw "bad-zero-root";
  }
  return result.length;
}

function gcArrayLengthAtEntry(marker, length) {
  const result = new Array(length);
  const trigger = new Array();
  if (marker.value < 0 || trigger.length !== 0) throw "bad-length-root";
  return result.length;
}

function gcCallArrayZero(fn, marker) {
  return fn(marker);
}

function gcCallArrayLength(fn, marker, length) {
  return fn(marker, length);
}

globalThis.__stackArrayWarmMarker = { value: 0 };
for (let warm = 0; warm < 5000; warm++) {
  gcArrayZeroAtEntry(__stackArrayWarmMarker);
  gcArrayLengthAtEntry(__stackArrayWarmMarker, 4);
  gcCallArrayZero(gcArrayZeroAtEntry, __stackArrayWarmMarker);
  gcCallArrayLength(gcArrayLengthAtEntry, __stackArrayWarmMarker, 4);
}
"#;

const ORDINARY_CONSTRUCTOR_SETUP: &str = r#"
function OrdinaryArrayField() {
  this.elms = new Array();
}

function constructOrdinaryArrayField(Ctor) {
  return new Ctor();
}

for (let warm = 0; warm < 5000; warm++) {
  constructOrdinaryArrayField(OrdinaryArrayField);
}
"#;

const ORDINARY_CONSTRUCTOR_PROBE: &str = r#"
JSON.stringify([
  constructOrdinaryArrayField(OrdinaryArrayField).elms.length,
  constructOrdinaryArrayField(OrdinaryArrayField).elms.length
]);
"#;

const NON_EXTENSIBLE_CONSTRUCTOR_SETUP: &str = r#"
function lockOrdinaryReceiver(receiver) {
  Object.preventExtensions(receiver);
}

function NonExtensibleOrdinaryField(lock) {
  lock(this);
  this.x = 1;
}

function constructNonExtensibleOrdinary(Ctor, lock) {
  return new Ctor(lock);
}

for (let warm = 0; warm < 5000; warm++) {
  lockOrdinaryReceiver({});
}
for (let warm = 0; warm < 5000; warm++) {
  constructNonExtensibleOrdinary(
    NonExtensibleOrdinaryField,
    lockOrdinaryReceiver
  );
}
"#;

const NON_EXTENSIBLE_CONSTRUCTOR_PROBE: &str = r#"
(() => {
  const first = constructNonExtensibleOrdinary(
    NonExtensibleOrdinaryField,
    lockOrdinaryReceiver
  );
  const second = constructNonExtensibleOrdinary(
    NonExtensibleOrdinaryField,
    lockOrdinaryReceiver
  );
  return JSON.stringify([
    Object.isExtensible(first),
    Object.hasOwn(first, "x"),
    typeof first.x,
    Object.isExtensible(second),
    Object.hasOwn(second, "x"),
    typeof second.x
  ]);
})();
"#;

const WIDE_ORDINARY_CONSTRUCTOR_SETUP: &str = r#"
function WideOrdinaryFields(value) {
  const adjusted = value + 0;
  this.a = adjusted;
  this.b = adjusted + 1;
  this.c = adjusted + 2;
  this.d = adjusted + 3;
}

function constructWideOrdinary(Ctor, value) {
  return new Ctor(value);
}

for (let warm = 0; warm < 5000; warm++) {
  constructWideOrdinary(WideOrdinaryFields, warm);
}
"#;

const WIDE_ORDINARY_CONSTRUCTOR_PROBE: &str = r#"
(() => {
  const first = constructWideOrdinary(WideOrdinaryFields, 10);
  const second = constructWideOrdinary(WideOrdinaryFields, 20);
  return JSON.stringify([
    first.a,
    first.b,
    first.c,
    first.d,
    Object.keys(first).join(","),
    second.a,
    second.b,
    second.c,
    second.d,
    Object.keys(second).join(",")
  ]);
})();
"#;

const TINY_CONSTRUCT_COST_SETUP: &str = r#"
function TinyConstructCost() {
  this.items = new Array();
}

function makeTinyConstructCost() {
  return new TinyConstructCost();
}

for (let warm = 0; warm < 5000; warm++) {
  makeTinyConstructCost();
}
"#;

#[derive(Clone, Copy, Debug)]
struct CounterDelta {
    generated_calls: u64,
    generated_call_deopts: u64,
    generated_template_entries: u64,
    generated_template_returns: u64,
    generated_template_deopts: u64,
    generated_template_throws: u64,
    generated_optimizing_entries: u64,
    generated_optimizing_returns: u64,
    generated_optimizing_deopts: u64,
    generated_optimizing_throws: u64,
    optimized_deopts: u64,
    to_rust_call_transitions: u64,
    alloc_value_stub_ok: u64,
    alloc_value_stub_miss: u64,
    alloc_value_stub_out_of_memory: u64,
    alloc_value_stub_other: u64,
    property_store_misses: u64,
    minor_gc_cycles: u64,
}

impl CounterDelta {
    fn between(before: RuntimeExecutionStats, after: RuntimeExecutionStats) -> Self {
        Self {
            generated_calls: after.jit_generated_calls - before.jit_generated_calls,
            generated_call_deopts: after.jit_generated_call_deopts
                - before.jit_generated_call_deopts,
            generated_template_entries: after.jit_generated_template_entries
                - before.jit_generated_template_entries,
            generated_template_returns: after.jit_generated_template_returns
                - before.jit_generated_template_returns,
            generated_template_deopts: after.jit_generated_template_deopts
                - before.jit_generated_template_deopts,
            generated_template_throws: after.jit_generated_template_throws
                - before.jit_generated_template_throws,
            generated_optimizing_entries: after.jit_generated_optimizing_entries
                - before.jit_generated_optimizing_entries,
            generated_optimizing_returns: after.jit_generated_optimizing_returns
                - before.jit_generated_optimizing_returns,
            generated_optimizing_deopts: after.jit_generated_optimizing_deopts
                - before.jit_generated_optimizing_deopts,
            generated_optimizing_throws: after.jit_generated_optimizing_throws
                - before.jit_generated_optimizing_throws,
            optimized_deopts: after.jit_optimized_deopts - before.jit_optimized_deopts,
            to_rust_call_transitions: after.jit_to_rust_call_transitions
                - before.jit_to_rust_call_transitions,
            alloc_value_stub_ok: after.jit_alloc_value_stub_ok - before.jit_alloc_value_stub_ok,
            alloc_value_stub_miss: after.jit_alloc_value_stub_miss
                - before.jit_alloc_value_stub_miss,
            alloc_value_stub_out_of_memory: after.jit_alloc_value_stub_out_of_memory
                - before.jit_alloc_value_stub_out_of_memory,
            alloc_value_stub_other: after.jit_alloc_value_stub_other
                - before.jit_alloc_value_stub_other,
            property_store_misses: after.property_store_misses - before.property_store_misses,
            minor_gc_cycles: after.gc_minor_cycles - before.gc_minor_cycles,
        }
    }

    fn generated_entries(self) -> u64 {
        self.generated_template_entries + self.generated_optimizing_entries
    }

    fn generated_returns(self) -> u64 {
        self.generated_template_returns + self.generated_optimizing_returns
    }

    fn generated_deopts(self) -> u64 {
        self.generated_template_deopts + self.generated_optimizing_deopts
    }

    fn generated_throws(self) -> u64 {
        self.generated_template_throws + self.generated_optimizing_throws
    }
}

fn runtime() -> Runtime {
    Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .expect("stack-owned ArrayConstruct runtime")
}

fn interpreter_runtime() -> Runtime {
    Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .expect("interpreter-only constructor transition runtime")
}

fn run(runtime: &mut Runtime, source: impl Into<String>, module: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source.into()), module)
        .unwrap_or_else(|error| panic!("stack-owned ArrayConstruct fixture {module}: {error:?}"))
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

fn function_id(artifacts: &JitArtifactBatch, module: &str, function_name: &str) -> u32 {
    artifacts
        .bundles()
        .iter()
        .find_map(|bundle| {
            let manifest = bundle.manifest();
            (manifest.module() == module
                && manifest.function_name() == function_name
                && manifest.entry() == JitDebugTarget::Entry)
                .then(|| manifest.function_id())
        })
        .unwrap_or_else(|| panic!("missing entry artifact for {module}:{function_name}"))
}

fn instruction_location(
    artifacts: &JitArtifactBatch,
    module: &str,
    function_name: &str,
    opcode: &str,
) -> (u32, u32) {
    let mut entry_bundles = artifacts.bundles().iter().filter(|bundle| {
        let manifest = bundle.manifest();
        manifest.module() == module
            && manifest.function_name() == function_name
            && manifest.entry() == JitDebugTarget::Entry
    });
    let first = entry_bundles
        .next()
        .unwrap_or_else(|| panic!("missing entry bundle for {module}:{function_name}"));
    let bytecode = std::str::from_utf8(
        first
            .file(JitArtifactFileName::Bytecode)
            .expect("entry bytecode artifact")
            .contents(),
    )
    .expect("UTF-8 bytecode artifact");
    let instruction = bytecode
        .lines()
        .find(|line| line.contains(&format!(" {opcode} ")))
        .unwrap_or_else(|| panic!("missing {opcode} in {module}:{function_name}"));
    let logical_pc = instruction
        .split_whitespace()
        .next()
        .and_then(|pc| pc.parse::<u32>().ok())
        .unwrap_or_else(|| panic!("invalid {opcode} PC: {instruction}"));
    let byte_pc = instruction
        .split_whitespace()
        .find_map(|field| field.strip_prefix("byte="))
        .and_then(|pc| pc.parse::<u32>().ok())
        .unwrap_or_else(|| panic!("invalid {opcode} byte PC: {instruction}"));
    (logical_pc, byte_pc)
}

fn array_construct_location(
    artifacts: &JitArtifactBatch,
    module: &str,
    function_name: &str,
) -> (u32, u32) {
    instruction_location(artifacts, module, function_name, "ArrayConstruct")
}

fn assert_array_construct_at_entry(
    artifacts: &JitArtifactBatch,
    module: &str,
    function_name: &str,
) {
    let construct_pc = array_construct_location(artifacts, module, function_name).0;
    assert!(
        construct_pc <= 1,
        "{module}:{function_name} must hit ArrayConstruct before any effectful body work; pc={construct_pc}"
    );
}

fn assert_machine_direct_edge(
    artifacts: &JitArtifactBatch,
    module: &str,
    caller_name: &str,
    callee_name: &str,
    call_kind: &str,
) {
    let callee_function_id = function_id(artifacts, module, callee_name);
    let mut machine_callers = 0;
    let mut matching_edge = false;
    let mut observed_callers = Vec::new();
    for bundle in artifacts.bundles() {
        let manifest = bundle.manifest();
        if manifest.module() == module && manifest.function_name() == caller_name {
            let backend = bundle
                .file(JitArtifactFileName::OptimizedIr)
                .and_then(|file| std::str::from_utf8(file.contents()).ok())
                .and_then(|text| text.lines().next())
                .unwrap_or("<template>");
            observed_callers.push((manifest.tier(), manifest.entry(), backend.to_owned()));
        }
        if manifest.module() != module
            || manifest.function_name() != caller_name
            || manifest.tier() != JitDebugTier::Optimizing
            || manifest.entry() != JitDebugTarget::Entry
            || !bundle
                .file(JitArtifactFileName::OptimizedIr)
                .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
        {
            continue;
        }
        machine_callers += 1;
        let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
        matching_edge |= relocations["relocations"]
            .as_array()
            .expect("relocation array")
            .iter()
            .any(|relocation| {
                let target = &relocation["target"];
                let direct = &target["directCall"];
                target["kind"] == "directCallEntryCell"
                    && direct["callKind"] == call_kind
                    && direct["argumentMode"] == "fixed"
                    && direct["targetFunctionId"].as_u64() == Some(u64::from(callee_function_id))
                    && direct["targetIndex"].as_u64() == Some(0)
                    && direct["targetCount"].as_u64() == Some(1)
            });
    }
    assert!(
        machine_callers > 0,
        "missing Machine IR entry artifact for {module}:{caller_name}; observed={observed_callers:?}"
    );
    assert!(
        matching_edge,
        "{module}:{caller_name} must directly enter {callee_name} through one fixed target"
    );
}

fn assert_machine_plain_direct_edge(
    artifacts: &JitArtifactBatch,
    module: &str,
    caller_name: &str,
    callee_name: &str,
) {
    assert_machine_direct_edge(artifacts, module, caller_name, callee_name, "plain");
}

fn assert_stack_owned_array_entry_edge(
    artifacts: &JitArtifactBatch,
    module: &str,
    caller_name: &str,
    callee_name: &str,
) {
    assert_array_construct_at_entry(artifacts, module, callee_name);
    assert_machine_plain_direct_edge(artifacts, module, caller_name, callee_name);
}

fn assert_array_construct_backend(
    artifacts: &JitArtifactBatch,
    module: &str,
    function_name: &str,
    expected_tier: JitDebugTier,
    machine: bool,
) {
    let expected_pc = u64::from(array_construct_location(artifacts, module, function_name).1);
    let mut observed = Vec::new();
    for bundle in artifacts.bundles().iter().filter(|bundle| {
        let manifest = bundle.manifest();
        manifest.module() == module
            && manifest.function_name() == function_name
            && manifest.entry() == JitDebugTarget::Entry
            && manifest.tier() == expected_tier
    }) {
        let is_machine = bundle
            .file(JitArtifactFileName::OptimizedIr)
            .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER));
        let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
        let has_stub = relocations["relocations"]
            .as_array()
            .expect("ArrayConstruct relocation array")
            .iter()
            .any(|relocation| {
                relocation["target"]["kind"] == "runtimeStub"
                    && relocation["target"]["name"] == "array_construct_alloc"
            });
        observed.push((is_machine, has_stub));
        if is_machine != machine || !has_stub {
            continue;
        }
        if machine {
            let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
            assert!(
                code_map["regions"]
                    .as_array()
                    .expect("Machine ArrayConstruct regions")
                    .iter()
                    .any(|region| {
                        region["kind"] == "machineArrayConstruct"
                            && region["bytePc"].as_u64() == Some(expected_pc)
                    }),
                "Machine ArrayConstruct must retain its exact bytecode region: {code_map}"
            );
        }
        return;
    }
    panic!(
        "missing {expected_tier:?} ArrayConstruct backend for {module}:{function_name}; \
         machine={machine} observed={observed:?}"
    );
}

fn assert_machine_array_field_constructor(
    artifacts: &JitArtifactBatch,
    module: &str,
    function_name: &str,
) {
    let array_byte_pc =
        u64::from(instruction_location(artifacts, module, function_name, "ArrayConstruct").1);
    let store_byte_pc =
        u64::from(instruction_location(artifacts, module, function_name, "StoreProperty").1);
    let bundle = artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == module
                && manifest.function_name() == function_name
                && manifest.tier() == JitDebugTier::Optimizing
                && manifest.entry() == JitDebugTarget::Entry
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
        })
        .unwrap_or_else(|| panic!("missing Machine constructor bundle for {function_name}"));
    let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let regions = code_map["regions"].as_array().expect("constructor regions");
    for (kind, byte_pc) in [
        ("machineArrayConstruct", array_byte_pc),
        ("machineCacheIrGuardExtensible", store_byte_pc),
        ("machineCacheIrStoreField", store_byte_pc),
        ("machineCacheIrPublishShape", store_byte_pc),
        ("machineCacheIrWriteBarrier", store_byte_pc),
    ] {
        assert!(
            regions.iter().any(|region| {
                region["kind"] == kind && region["bytePc"].as_u64() == Some(byte_pc)
            }),
            "{function_name} must retain {kind} at bytePc={byte_pc}: {code_map}"
        );
    }
}

fn assert_machine_constructor_field_regions(
    artifacts: &JitArtifactBatch,
    module: &str,
    function_name: &str,
    expected_store_count: usize,
) {
    let entry_bundle = artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == module
                && manifest.function_name() == function_name
                && manifest.entry() == JitDebugTarget::Entry
        })
        .unwrap_or_else(|| panic!("missing entry bundle for {module}:{function_name}"));
    let bytecode = std::str::from_utf8(
        entry_bundle
            .file(JitArtifactFileName::Bytecode)
            .expect("constructor bytecode artifact")
            .contents(),
    )
    .expect("UTF-8 constructor bytecode artifact");
    let store_byte_pcs = bytecode
        .lines()
        .filter(|line| line.contains(" StoreProperty "))
        .map(|instruction| {
            instruction
                .split_whitespace()
                .find_map(|field| field.strip_prefix("byte="))
                .and_then(|pc| pc.parse::<u32>().ok())
                .unwrap_or_else(|| panic!("invalid StoreProperty byte PC: {instruction}"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        store_byte_pcs.len(),
        expected_store_count,
        "unexpected StoreProperty count for {module}:{function_name}: {bytecode}"
    );

    let mut observed_machine_regions = Vec::new();
    let mut observed_bundles = Vec::new();
    for bundle in artifacts.bundles().iter().filter(|bundle| {
        let manifest = bundle.manifest();
        manifest.module() == module
            && manifest.function_name() == function_name
            && manifest.entry() == JitDebugTarget::Entry
    }) {
        let manifest = bundle.manifest();
        let backend = bundle
            .file(JitArtifactFileName::OptimizedIr)
            .and_then(|file| std::str::from_utf8(file.contents()).ok())
            .and_then(|text| text.lines().next())
            .unwrap_or("<template>");
        observed_bundles.push((manifest.tier(), backend.to_owned()));
    }
    for bundle in artifacts.bundles().iter().filter(|bundle| {
        let manifest = bundle.manifest();
        manifest.module() == module
            && manifest.function_name() == function_name
            && manifest.tier() == JitDebugTier::Optimizing
            && manifest.entry() == JitDebugTarget::Entry
            && bundle
                .file(JitArtifactFileName::OptimizedIr)
                .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
    }) {
        let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
        let regions = code_map["regions"]
            .as_array()
            .expect("constructor code-map regions");
        let store_effect_byte_pcs = regions
            .iter()
            .filter(|region| region["kind"] == "machineCacheIrStoreField")
            .filter_map(|region| region["bytePc"].as_u64())
            .filter_map(|byte_pc| u32::try_from(byte_pc).ok())
            .collect::<Vec<_>>();
        if store_byte_pcs.iter().all(|byte_pc| {
            store_effect_byte_pcs
                .iter()
                .any(|effect_pc| effect_pc == byte_pc)
        }) {
            return;
        }
        observed_machine_regions.push(store_effect_byte_pcs);
    }
    panic!(
        "missing complete Machine constructor field program for {module}:{function_name}; \
         stores={store_byte_pcs:?} observed={observed_machine_regions:?} \
         bundles={observed_bundles:?}"
    );
}

fn assert_clean_generated_returns(delta: CounterDelta, expected_calls: u64) {
    assert_eq!(delta.generated_calls, expected_calls, "{delta:?}");
    assert_eq!(delta.generated_entries(), expected_calls, "{delta:?}");
    assert_eq!(delta.generated_returns(), expected_calls, "{delta:?}");
    assert_eq!(delta.generated_call_deopts, 0, "{delta:?}");
    assert_eq!(delta.generated_deopts(), 0, "{delta:?}");
    assert_eq!(delta.generated_throws(), 0, "{delta:?}");
    assert_eq!(delta.optimized_deopts, 0, "{delta:?}");
    assert_eq!(delta.to_rust_call_transitions, 0, "{delta:?}");
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

#[test]
fn zero_and_int32_length_constructs_stay_in_stack_owned_generated_callees() {
    const MODULE: &str = "jit-stack-owned-array-construct-normal.js";
    let mut runtime = runtime();
    let setup = runtime
        .run_script(SourceInput::from_javascript(NORMAL_SETUP), MODULE)
        .expect("normal ArrayConstruct setup");
    let artifacts = setup.jit_artifacts().expect("normal setup artifacts");
    assert_stack_owned_array_entry_edge(artifacts, MODULE, "callArrayZero", "arrayZeroAtEntry");
    assert_stack_owned_array_entry_edge(artifacts, MODULE, "callArrayLength", "arrayLengthAtEntry");
    assert_array_construct_backend(
        artifacts,
        MODULE,
        "arrayZeroAtEntry",
        JitDebugTier::Optimizing,
        true,
    );
    assert_array_construct_backend(
        artifacts,
        MODULE,
        "arrayLengthAtEntry",
        JitDebugTier::Optimizing,
        true,
    );
    drop(setup);

    let before = runtime.execution_stats();
    let completion = run(&mut runtime, NORMAL_PROBE, "jit-stack-owned-array-probe.js");
    let delta = CounterDelta::between(before, runtime.execution_stats());
    assert_eq!(completion, "[0,4,0,7]");
    assert_clean_generated_returns(delta, 4);
    assert_eq!(delta.generated_template_entries, 0, "{delta:?}");
    assert!(delta.generated_optimizing_entries > 0, "{delta:?}");
    assert!(
        delta.alloc_value_stub_ok >= 4,
        "each ArrayConstruct must cross its typed allocating boundary: {delta:?}"
    );
    assert_eq!(delta.alloc_value_stub_miss, 0, "{delta:?}");
    assert_eq!(delta.alloc_value_stub_out_of_memory, 0, "{delta:?}");
    assert_eq!(delta.alloc_value_stub_other, 0, "{delta:?}");
}

#[test]
fn invalid_length_throws_range_error_once_and_reuses_the_generated_caller() {
    const MODULE: &str = "jit-stack-owned-array-invalid-length.js";
    let mut runtime = runtime();
    let setup = runtime
        .run_script(SourceInput::from_javascript(INVALID_LENGTH_SETUP), MODULE)
        .expect("invalid-length setup");
    let artifacts = setup.jit_artifacts().expect("invalid-length artifacts");
    assert_machine_plain_direct_edge(
        artifacts,
        MODULE,
        "callInvalidArrayLength",
        "invalidArrayLength",
    );
    let _construct_location = array_construct_location(artifacts, MODULE, "invalidArrayLength");
    drop(setup);

    let before = runtime.execution_stats();
    let completion = run(
        &mut runtime,
        INVALID_LENGTH_PROBE,
        "jit-stack-owned-array-invalid-length-probe.js",
    );
    let delta = CounterDelta::between(before, runtime.execution_stats());

    assert_eq!(completion, r#"["RangeError","Invalid array length",1,3,2]"#);
    assert_eq!(delta.generated_calls, 2, "{delta:?}");
    assert_eq!(delta.generated_entries(), 2, "{delta:?}");
    assert_eq!(delta.generated_returns(), 1, "{delta:?}");
    // The cold-deoptimized callee resumes to a throw: both transitions count.
    assert_eq!(delta.generated_throws(), 1, "{delta:?}");
    assert_eq!(delta.generated_call_deopts, 1, "{delta:?}");
    assert_eq!(delta.generated_deopts(), 1, "{delta:?}");
    assert_eq!(delta.optimized_deopts, 1, "{delta:?}");
    assert_eq!(delta.to_rust_call_transitions, 0, "{delta:?}");
    assert_eq!(delta.alloc_value_stub_ok, 1, "{delta:?}");
    assert_eq!(delta.alloc_value_stub_miss, 1, "{delta:?}");
    assert_eq!(delta.alloc_value_stub_out_of_memory, 0, "{delta:?}");
    assert_eq!(delta.alloc_value_stub_other, 0, "{delta:?}");
}

#[test]
fn ordinary_constructor_array_and_field_transition_stay_generated() {
    const MODULE: &str = "jit-stack-owned-array-ordinary-constructor.js";
    let mut runtime = runtime();
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(ORDINARY_CONSTRUCTOR_SETUP),
            MODULE,
        )
        .expect("ordinary ArrayConstruct constructor setup");
    let artifacts = setup
        .jit_artifacts()
        .expect("ordinary ArrayConstruct constructor artifacts");
    assert_machine_direct_edge(
        artifacts,
        MODULE,
        "constructOrdinaryArrayField",
        "OrdinaryArrayField",
        "construct",
    );
    assert_machine_array_field_constructor(artifacts, MODULE, "OrdinaryArrayField");
    drop(setup);

    let before = runtime.execution_stats();
    let completion = run(
        &mut runtime,
        ORDINARY_CONSTRUCTOR_PROBE,
        "jit-stack-owned-array-ordinary-constructor-probe.js",
    );
    let delta = CounterDelta::between(before, runtime.execution_stats());
    assert_eq!(completion, "[0,0]");
    assert_clean_generated_returns(delta, 2);
    assert_eq!(delta.alloc_value_stub_ok, 2, "{delta:?}");
    assert_eq!(delta.property_store_misses, 0, "{delta:?}");
}

#[test]
fn ordinary_constructor_transition_cannot_append_to_a_non_extensible_receiver() {
    const MODULE: &str = "jit-ordinary-constructor-non-extensible.js";
    const PROBE_MODULE: &str = "jit-ordinary-constructor-non-extensible-probe.js";

    let mut oracle = interpreter_runtime();
    run(&mut oracle, NON_EXTENSIBLE_CONSTRUCTOR_SETUP, MODULE);
    let expected = run(&mut oracle, NON_EXTENSIBLE_CONSTRUCTOR_PROBE, PROBE_MODULE);
    assert_eq!(
        expected,
        r#"[false,false,"undefined",false,false,"undefined"]"#
    );

    let mut runtime = runtime();
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(NON_EXTENSIBLE_CONSTRUCTOR_SETUP),
            MODULE,
        )
        .expect("non-extensible ordinary constructor setup");
    let artifacts = setup
        .jit_artifacts()
        .expect("non-extensible ordinary constructor artifacts");
    assert_machine_direct_edge(
        artifacts,
        MODULE,
        "constructNonExtensibleOrdinary",
        "NonExtensibleOrdinaryField",
        "construct",
    );
    assert_machine_constructor_field_regions(artifacts, MODULE, "NonExtensibleOrdinaryField", 1);
    drop(setup);

    let completion = run(&mut runtime, NON_EXTENSIBLE_CONSTRUCTOR_PROBE, PROBE_MODULE);
    assert_eq!(completion, expected);
}

#[test]
fn wide_ordinary_constructor_transitions_preserve_the_reserved_slab() {
    const MODULE: &str = "jit-wide-ordinary-constructor-fields.js";
    let mut runtime = runtime();
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(WIDE_ORDINARY_CONSTRUCTOR_SETUP),
            MODULE,
        )
        .expect("wide ordinary constructor setup");
    let artifacts = setup
        .jit_artifacts()
        .expect("wide ordinary constructor artifacts");
    assert_machine_direct_edge(
        artifacts,
        MODULE,
        "constructWideOrdinary",
        "WideOrdinaryFields",
        "construct",
    );
    assert_machine_constructor_field_regions(artifacts, MODULE, "WideOrdinaryFields", 4);
    drop(setup);

    let before = runtime.execution_stats();
    let completion = run(
        &mut runtime,
        WIDE_ORDINARY_CONSTRUCTOR_PROBE,
        "jit-wide-ordinary-constructor-fields-probe.js",
    );
    let delta = CounterDelta::between(before, runtime.execution_stats());
    assert_eq!(
        completion,
        r#"[10,11,12,13,"a,b,c,d",20,21,22,23,"a,b,c,d"]"#
    );
    assert_clean_generated_returns(delta, 2);
    assert_eq!(delta.property_store_misses, 0, "{delta:?}");

    runtime
        .force_gc()
        .expect("wide constructor transition roots must unlink after return");
    let reused = run(
        &mut runtime,
        r#"
const value = constructWideOrdinary(WideOrdinaryFields, 30);
JSON.stringify([value.a, value.b, value.c, value.d, Object.keys(value).join(",")]);
"#,
        "jit-wide-ordinary-constructor-fields-reuse.js",
    );
    assert_eq!(reused, r#"[30,31,32,33,"a,b,c,d"]"#);
}

#[test]
fn tiny_base_constructor_uses_machine_or_template_without_a_legacy_optimizer() {
    const MODULE: &str = "jit-tiny-construct-cost-model.js";
    let mut runtime = runtime();
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(TINY_CONSTRUCT_COST_SETUP),
            MODULE,
        )
        .expect("tiny constructor cost-model setup");
    let artifacts = setup
        .jit_artifacts()
        .expect("tiny constructor cost-model artifacts");
    let caller_bundles = artifacts
        .bundles()
        .iter()
        .filter(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == MODULE
                && manifest.function_name() == "makeTinyConstructCost"
                && manifest.entry() == JitDebugTarget::Entry
        })
        .collect::<Vec<_>>();
    assert!(
        caller_bundles
            .iter()
            .any(|bundle| bundle.manifest().tier() == JitDebugTier::Template),
        "tiny constructor caller must retain a Template baseline"
    );
    assert!(
        caller_bundles.iter().all(|bundle| {
            bundle.manifest().tier() != JitDebugTier::Optimizing
                || bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
        }),
        "every optimizing artifact must come from Machine IR"
    );
    drop(setup);

    let result = run(
        &mut runtime,
        r#"
const value = makeTinyConstructCost();
JSON.stringify([value.items.length, Object.keys(value).join(",")]);
"#,
        "jit-tiny-construct-policy-probe.js",
    );
    assert_eq!(result, r#"[0,"items"]"#);
}

#[test]
fn array_results_and_live_arguments_survive_moving_gc_and_full_gc_reuse() {
    const MODULE: &str = "jit-stack-owned-array-gc.js";
    let mut runtime = runtime();
    let setup = runtime
        .run_script(SourceInput::from_javascript(GC_SETUP), MODULE)
        .expect("ArrayConstruct GC setup");
    let artifacts = setup.jit_artifacts().expect("ArrayConstruct GC artifacts");
    assert_stack_owned_array_entry_edge(artifacts, MODULE, "gcCallArrayZero", "gcArrayZeroAtEntry");
    assert_stack_owned_array_entry_edge(
        artifacts,
        MODULE,
        "gcCallArrayLength",
        "gcArrayLengthAtEntry",
    );
    drop(setup);

    let iterations = if gc_stress_stride() == 0 {
        150_000_u32
    } else {
        64_u32
    };
    let probe = format!(
        r#"
let checksum = 0;
for (let i = 0; i < {iterations}; i++) {{
  const marker = {{ value: i }};
  const zeroLength = gcCallArrayZero(gcArrayZeroAtEntry, marker);
  const sizedLength = gcCallArrayLength(gcArrayLengthAtEntry, marker, 4);
  if (zeroLength !== 0 || sizedLength !== 4 || marker.value !== i) {{
    throw "corrupt ArrayConstruct root";
  }}
  checksum += marker.value + zeroLength + sizedLength;
}}
String(checksum);
"#
    );
    let expected_checksum =
        u64::from(iterations) * u64::from(iterations - 1) / 2 + u64::from(iterations) * 4;
    let before = runtime.execution_stats();
    let completion = run(&mut runtime, probe, "jit-stack-owned-array-gc-probe.js");
    let delta = CounterDelta::between(before, runtime.execution_stats());

    assert_eq!(completion, expected_checksum.to_string());
    assert_clean_generated_returns(delta, u64::from(iterations) * 2);
    assert!(
        delta.alloc_value_stub_ok >= u64::from(iterations) * 4,
        "both Array results must remain rooted across a second allocation: {delta:?}"
    );
    assert!(
        delta.minor_gc_cycles > 0,
        "ArrayConstruct must collect while stack-owned values are live: {delta:?}"
    );

    runtime
        .force_gc()
        .expect("completed ArrayConstruct calls must unlink native root records");
    let before_reuse = runtime.execution_stats();
    let reused = run(
        &mut runtime,
        r#"
const marker = { value: 9 };
JSON.stringify([
  gcCallArrayZero(gcArrayZeroAtEntry, marker),
  gcCallArrayLength(gcArrayLengthAtEntry, marker, 4),
  marker.value
]);
"#,
        "jit-stack-owned-array-gc-reuse.js",
    );
    let reuse_delta = CounterDelta::between(before_reuse, runtime.execution_stats());
    assert_eq!(reused, "[0,4,9]");
    assert_clean_generated_returns(reuse_delta, 2);
}
