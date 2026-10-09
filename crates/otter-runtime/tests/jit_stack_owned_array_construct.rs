//! Stack-owned generated-callee `ArrayConstruct` coverage.
//!
//! # Contents
//! - Zero-argument and one-Int32-length Array construction at callee entry.
//! - Invalid-length exact deoptimization and canonical `RangeError` without replay.
//! - Moving-GC preservation of live arguments and an earlier Array result.
//! - Independently admitted Graph caller entries, exact call-cell artifacts,
//!   and complete interpreter traces proving native execution.
//! - Cold exact-generation and aggregate exit accounting without Template entry.
//! - Exact ordinary function-constructor field-transition publication.
//! - Retained first-seven cells and finalized native receiver geometry after full GC.
//! - Pre-commit extensibility guards and out-of-line field-slab preservation.
//!
//! # Invariants
//! - Successful callee-entry cases allocate before any effectful body work.
//!   The invalid-length callee records one preceding effect to detect replay.
//! - Every probe enters that callee through generated stack-owned linkage.
//! - A successful allocation returns once; pre-effect misses deopt exactly once.
//! - Completed calls unlink their native roots and leave the caller reusable.
//! - Template generations count their entries and returns; graph generations
//!   count only their exits. Graph execution requires exact current call
//!   artifacts, absent subject interpreter dispatch, typed allocation hits,
//!   and unchanged installed generations.
//!
//! # See also
//! - `otter_vm::runtime_activation` for stack-owned semantic operations.
//! - The target `otter-jit` JS-call encoder for private generated calls.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use otter_bytecode::Op;
use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest,
    JitDebugTarget, JitDebugTier, JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{CodeLifetimeState, ExitAction, ExitReason, NativeFrameKind},
};

#[path = "jit_stack_owned_array_construct/receiver.rs"]
mod receiver;

#[path = "jit_stack_owned_array_construct/first_seven.rs"]
mod first_seven;

const GRAPH_IR_HEADER: &[u8] = b"; otter graph\n";

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

// Callee promotion from a native caller is sampled at batched backedge polls
// (one per 4,096 compiled back-edges); these warm loops span several polls.
const ORDINARY_CONSTRUCTOR_SETUP: &str = r#"
function OrdinaryArrayField() {
  // Reading its actual arguments keeps the constructor out of its caller's
  // body, so the probe enters its own generation.
  if (arguments.length !== 0) throw "ordinary arity";
  this.elms = new Array();
}

function constructOrdinaryArrayField(Ctor) {
  return new Ctor();
}

for (let warm = 0; warm < 20000; warm++) {
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
  // Reading its actual arguments keeps the constructor out of its caller's
  // body, so the caller enters it through its entry cell.
  if (arguments.length !== 1) throw "non-extensible arity";
  lock(this);
  this.x = 1;
}

function constructNonExtensibleOrdinary(Ctor, lock) {
  return new Ctor(lock);
}

for (let warm = 0; warm < 5000; warm++) {
  lockOrdinaryReceiver({});
}
for (let warm = 0; warm < 20000; warm++) {
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
  // Reading its actual arguments keeps the constructor out of its caller's
  // body, so the caller enters it through its entry cell.
  if (arguments.length !== 1) throw "wide arity";
  const adjusted = value + 0;
  this.a = adjusted;
  this.b = adjusted + 1;
  this.c = adjusted + 2;
  this.d = adjusted + 3;
}

function constructWideOrdinary(Ctor, value) {
  return new Ctor(value);
}

for (let warm = 0; warm < 20000; warm++) {
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
    generated_optimizing_deopts: u64,
    optimized_deopts: u64,
    optimized_osr_entries: u64,
    code_generations: u64,
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
            generated_optimizing_deopts: after.jit_generated_optimizing_deopts
                - before.jit_generated_optimizing_deopts,
            optimized_deopts: after.jit_optimized_deopts - before.jit_optimized_deopts,
            optimized_osr_entries: after.jit_optimized_osr_entries
                - before.jit_optimized_osr_entries,
            code_generations: after.jit_code_generations - before.jit_code_generations,
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

    fn generated_deopts(self) -> u64 {
        self.generated_template_deopts + self.generated_optimizing_deopts
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

#[derive(Debug, Default)]
struct ArrayProbeTrace {
    names: BTreeMap<u32, String>,
    recording: bool,
    steps: usize,
    ticks: Vec<(u32, usize, u32, Op)>,
}

struct ArrayProbeTracer(Arc<Mutex<ArrayProbeTrace>>);

impl StepTracer for ArrayProbeTracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self.0.lock().unwrap();
        trace
            .names
            .entry(event.function_id)
            .or_insert_with(|| event.function_name.to_owned());
        if trace.recording {
            trace.steps += 1;
            if trace.ticks.len() < 256 {
                trace.ticks.push((
                    event.function_id,
                    event.frame_depth,
                    event.byte_pc,
                    event.op,
                ));
            }
        }
    }
}

fn traced_function_names<'a>(trace: &ArrayProbeTrace, names: &[&'a str]) -> Vec<(&'a str, u32)> {
    names
        .iter()
        .map(|&name| {
            let (&fid, _) = trace
                .names
                .iter()
                .find(|(_, observed)| observed.as_str() == name)
                .unwrap_or_else(|| panic!("warm interpreter dispatch must identify {name}"));
            (name, fid)
        })
        .collect()
}

fn has_current_graph(generation: &JitCodeGenerationSnapshot, fid: u32) -> bool {
    generation.function_id == fid
        && generation.tier == NativeFrameKind::Optimizing
        && generation.lifecycle == CodeLifetimeState::Installed
        && generation.linked
}

fn current_graph(runtime: &Runtime, fid: u32) -> JitCodeGenerationSnapshot {
    let current: Vec<_> = runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .filter(|generation| has_current_graph(generation, fid))
        .collect();
    assert_eq!(
        current.len(),
        1,
        "one installed current Graph for fid={fid}; observed={current:?}"
    );
    current.into_iter().next().unwrap()
}

fn admit_normal_graph_entries(
    runtime: &mut Runtime,
    artifacts: JitArtifactBatch,
    names: &[(&str, u32)],
) -> JitArtifactBatch {
    admit_graph_entries(
        runtime,
        artifacts,
        names,
        "callArrayZero(arrayZeroAtEntry);\ncallArrayLength(arrayLengthAtEntry, 4);\n",
        "jit-stack-owned-array-own-admission",
    )
}

fn admit_graph_entries(
    runtime: &mut Runtime,
    mut artifacts: JitArtifactBatch,
    names: &[(&str, u32)],
    calls: &str,
    module_prefix: &str,
) -> JitArtifactBatch {
    // A compiled warm loop may splice these wrappers and stop crediting their
    // own source work. Fresh straight-line scripts let their ordinary entries
    // execute and earn promotion independently. No script contains a backedge.
    const BATCHES: usize = 128;
    const PAIRS_PER_BATCH: usize = 16;
    for batch in 0..=BATCHES {
        let generations = runtime.jit_code_generation_snapshot();
        if names.iter().all(|(_, fid)| {
            generations
                .iter()
                .any(|generation| has_current_graph(generation, *fid))
        }) {
            assert!(!artifacts.truncated(), "complete own-entry artifacts");
            return artifacts;
        }
        if batch == BATCHES {
            break;
        }
        let mut admission = runtime
            .run_script(
                SourceInput::from_javascript(calls.repeat(PAIRS_PER_BATCH)),
                &format!("{module_prefix}-{batch}.js"),
            )
            .expect("independent ordinary ArrayConstruct wrapper admission");
        artifacts = artifacts.merged(admission.take_jit_artifacts().unwrap());
    }
    let generations: Vec<_> = runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .filter(|generation| names.iter().any(|(_, fid)| *fid == generation.function_id))
        .collect();
    panic!(
        "bounded independent calls did not admit every Graph entry; \
         max_calls_per_wrapper={}; names={names:?}; generations={generations:?}",
        BATCHES * PAIRS_PER_BATCH
    );
}

fn current_graph_entry<'a>(
    artifacts: &'a JitArtifactBatch,
    generation: &JitCodeGenerationSnapshot,
) -> &'a JitArtifactBundle {
    let bundle = artifacts
        .bundles()
        .iter()
        .find(|bundle| bundle.manifest().code_object_id() == generation.code_object_id)
        .expect("the exact current generation owns its emitted artifact");
    assert_eq!(bundle.manifest().function_id(), generation.function_id);
    assert_eq!(bundle.manifest().entry(), JitDebugTarget::Entry);
    assert!(is_graph_bundle(bundle));
    bundle
}

fn assert_current_graph_call(
    artifacts: &JitArtifactBatch,
    caller: &JitCodeGenerationSnapshot,
    callee: &JitCodeGenerationSnapshot,
) {
    let bundle = current_graph_entry(artifacts, caller);
    current_graph_entry(artifacts, callee);
    let map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let calls: Vec<_> = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["operation"]
                .as_str()
                .is_some_and(|operation| operation.contains("CallJs"))
        })
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "the caller retains the callee's own activation"
    );
    let call = calls[0];
    assert_eq!(
        call["functionId"].as_u64(),
        Some(u64::from(caller.function_id))
    );
    let start = call["startOffset"].as_u64().unwrap();
    let end = call["endOffset"].as_u64().unwrap();
    assert!(start < end, "the current own call has emitted instructions");
    let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
    let linkage_bytes = if cfg!(target_arch = "aarch64") { 12 } else { 6 };
    let entries: Vec<_> = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|relocation| {
            relocation["target"]["kind"] == "functionEntryCell"
                && relocation["target"]["functionId"].as_u64()
                    == Some(u64::from(callee.function_id))
                && relocation["startOffset"]
                    .as_u64()
                    .is_some_and(|offset| offset >= start)
                && relocation["endOffset"]
                    .as_u64()
                    .is_some_and(|offset| offset + linkage_bytes <= end)
        })
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "the own CallJs loads this callee's permanent entry cell"
    );
    let load_offset = entries[0]["endOffset"].as_u64().unwrap();
    #[cfg(target_arch = "aarch64")]
    {
        assert_eq!(entries[0]["register"].as_u64(), Some(8));
        let assembly = std::str::from_utf8(
            bundle
                .file(JitArtifactFileName::Assembly)
                .unwrap()
                .contents(),
        )
        .unwrap();
        for (index, instruction) in ["ldr x8, [x8]", "ldr x16, [x8]", "blr x16"]
            .into_iter()
            .enumerate()
        {
            let prefix = format!("+0x{:08x}:", load_offset + index as u64 * 4);
            assert!(
                assembly
                    .lines()
                    .any(|line| line.starts_with(&prefix) && line.ends_with(instruction)),
                "the exact own CallJs must load its current generation: code={} offset={prefix} instruction={instruction}",
                caller.code_object_id
            );
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        assert_eq!(entries[0]["register"].as_u64(), Some(11));
        let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
        let offset = usize::try_from(load_offset).unwrap();
        assert_eq!(
            &code[offset..offset + 6],
            &[0x4d, 0x8b, 0x0b, 0x41, 0xff, 0x11],
            "the exact own CallJs must load r9 from its cell and call [r9]: code={}",
            caller.code_object_id
        );
    }
}

fn assert_complete_probe_trace(trace: &ArrayProbeTrace) {
    assert_eq!(
        trace.steps,
        trace.ticks.len(),
        "complete isolated probe trace"
    );
    assert!(trace.ticks.iter().any(|(fid, _, _, op)| {
        trace.names[fid] == "<main>" && matches!(op, Op::Call | Op::CallWithThis)
    }));
    assert!(trace.ticks.iter().any(|(fid, _, _, op)| {
        trace.names[fid] == "<main>"
            && matches!(op, Op::Return | Op::ReturnUndefined | Op::ReturnValue)
    }));
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

/// Whether `bundle` holds code from the graph optimizing pipeline.
fn is_graph_bundle(bundle: &JitArtifactBundle) -> bool {
    bundle.manifest().tier() == JitDebugTier::Optimizing
        && bundle
            .file(JitArtifactFileName::OptimizedIr)
            .is_some_and(|file| file.contents().starts_with(GRAPH_IR_HEADER))
}

/// The first optimized-IR line of `bundle`, or `<template>` without one.
fn backend(bundle: &JitArtifactBundle) -> String {
    bundle
        .file(JitArtifactFileName::OptimizedIr)
        .and_then(|file| std::str::from_utf8(file.contents()).ok())
        .and_then(|text| text.lines().next())
        .unwrap_or("<template>")
        .to_owned()
}

fn assert_direct_edge(
    artifacts: &JitArtifactBatch,
    module: &str,
    caller_name: &str,
    callee_name: &str,
) {
    let callee_function_id = function_id(artifacts, module, callee_name);
    let mut graph_callers = 0;
    let mut matching_edge = false;
    let mut observed_callers = Vec::new();
    for bundle in artifacts.bundles() {
        let manifest = bundle.manifest();
        if manifest.module() == module && manifest.function_name() == caller_name {
            observed_callers.push((manifest.tier(), manifest.entry(), backend(bundle)));
        }
        // The caller may also be spliced into the OSR body of its own hot
        // loop; that body then owns the generated call edge.
        if manifest.module() != module || !is_graph_bundle(bundle) {
            continue;
        }
        graph_callers += 1;
        let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
        matching_edge |= relocations["relocations"]
            .as_array()
            .expect("relocation array")
            .iter()
            .any(|relocation| {
                let target = &relocation["target"];
                target["kind"] == "functionEntryCell"
                    && target["functionId"].as_u64() == Some(u64::from(callee_function_id))
            });
    }
    assert!(
        graph_callers > 0,
        "missing graph IR artifact in {module}; {caller_name} observed={observed_callers:?}"
    );
    assert!(
        matching_edge,
        "{module}:{caller_name} (or the body inlining it) must directly enter {callee_name} \
         through its entry cell"
    );
}

fn assert_stack_owned_array_entry_edge(
    artifacts: &JitArtifactBatch,
    module: &str,
    caller_name: &str,
    callee_name: &str,
) {
    assert_array_construct_at_entry(artifacts, module, callee_name);
    assert_direct_edge(artifacts, module, caller_name, callee_name);
}

/// Panic unless `module:function_name` has a graph-compiled entry bundle.
fn assert_graph_entry(artifacts: &JitArtifactBatch, module: &str, function_name: &str) {
    let mut observed_bundles = Vec::new();
    for bundle in artifacts.bundles().iter().filter(|bundle| {
        let manifest = bundle.manifest();
        manifest.module() == module
            && manifest.function_name() == function_name
            && manifest.entry() == JitDebugTarget::Entry
    }) {
        if is_graph_bundle(bundle) {
            return;
        }
        observed_bundles.push((bundle.manifest().tier(), backend(bundle)));
    }
    panic!("missing graph entry bundle for {module}:{function_name}; bundles={observed_bundles:?}");
}

fn assert_graph_constructor_fields(
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
    let store_count = bytecode
        .lines()
        .filter(|line| line.contains(" StoreProperty "))
        .count();
    assert_eq!(
        store_count, expected_store_count,
        "unexpected StoreProperty count for {module}:{function_name}: {bytecode}"
    );
    assert_graph_entry(artifacts, module, function_name);
}

/// `template_entries` Template generations were entered and each returned;
/// nothing exited.
fn assert_clean_generated_returns(delta: CounterDelta, template_entries: u64) {
    assert_eq!(delta.generated_calls, template_entries, "{delta:?}");
    assert_eq!(
        delta.generated_template_entries, template_entries,
        "{delta:?}"
    );
    assert_eq!(
        delta.generated_template_returns, template_entries,
        "{delta:?}"
    );
    assert_no_exits(delta);
}

/// No generated frame exited or crossed into a rooted runtime call.
fn assert_no_exits(delta: CounterDelta) {
    assert_eq!(delta.generated_call_deopts, 0, "{delta:?}");
    assert_eq!(delta.generated_deopts(), 0, "{delta:?}");
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
    let trace = Arc::new(Mutex::new(ArrayProbeTrace::default()));
    runtime.set_tracer(Some(Box::new(ArrayProbeTracer(trace.clone()))));
    let mut setup = runtime
        .run_script(SourceInput::from_javascript(NORMAL_SETUP), MODULE)
        .expect("normal ArrayConstruct setup");
    let names = traced_function_names(
        &trace.lock().unwrap(),
        &[
            "callArrayZero",
            "arrayZeroAtEntry",
            "callArrayLength",
            "arrayLengthAtEntry",
        ],
    );
    let artifacts =
        admit_normal_graph_entries(&mut runtime, setup.take_jit_artifacts().unwrap(), &names);
    let generations: Vec<_> = names
        .iter()
        .map(|(_, fid)| current_graph(&runtime, *fid))
        .collect();
    assert_current_graph_call(&artifacts, &generations[0], &generations[1]);
    assert_current_graph_call(&artifacts, &generations[2], &generations[3]);
    assert_array_construct_at_entry(&artifacts, MODULE, "arrayZeroAtEntry");
    assert_array_construct_at_entry(&artifacts, MODULE, "arrayLengthAtEntry");
    for callee in [&generations[1], &generations[3]] {
        let relocations = artifact_json(
            current_graph_entry(&artifacts, callee),
            JitArtifactFileName::Relocations,
        );
        assert!(
            relocations["relocations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|relocation| {
                    relocation["target"]["kind"] == "runtimeStub"
                        && relocation["target"]["name"] == "array_construct_alloc"
                })
        );
    }
    drop(setup);

    trace.lock().unwrap().recording = true;
    let before = runtime.execution_stats();
    let probe = runtime
        .run_script(
            SourceInput::from_javascript(NORMAL_PROBE),
            "jit-stack-owned-array-probe.js",
        )
        .expect("normal ArrayConstruct probe");
    let delta = CounterDelta::between(before, runtime.execution_stats());
    assert_eq!(probe.completion_string(), "[0,4,0,7]");
    let trace = trace.lock().unwrap();
    assert_complete_probe_trace(&trace);
    assert_eq!(
        trace
            .ticks
            .iter()
            .filter(|(fid, _, _, op)| trace.names[fid] == "<main>" && *op == Op::Call)
            .count(),
        4,
        "the interpreted probe independently dispatches all four wrapper calls"
    );
    for generation in &generations {
        assert!(
            !trace
                .ticks
                .iter()
                .any(|(fid, _, _, _)| *fid == generation.function_id),
            "the exact admitted caller/callee must execute natively: {generation:?}"
        );
        let after = current_graph(&runtime, generation.function_id);
        assert_eq!(after.code_object_id, generation.code_object_id);
        assert_eq!(after.generated_entries, 0);
        assert_eq!(after.generated_returns, 0);
        assert_eq!(after.generated_deopts, generation.generated_deopts);
        assert_eq!(
            after.active_count, 0,
            "completed native calls release their entry leases"
        );
    }
    drop(trace);
    let report = probe.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    assert!(!report.events().iter().any(|event| matches!(
        event,
        JitDebugEvent::Bail { .. }
            | JitDebugEvent::EnteredGenerationDeopt { .. }
            | JitDebugEvent::InlineDeoptFrame { .. }
            | JitDebugEvent::CompilePrepared { .. }
    )));
    // All four exact own entries are Graph code. Their native execution is
    // established above; that backend deliberately omits hot entry counters.
    assert_clean_generated_returns(delta, 0);
    assert_eq!(delta.code_generations, 0, "{delta:?}");
    assert_eq!(delta.optimized_osr_entries, 0, "{delta:?}");
    assert_eq!(
        delta.alloc_value_stub_ok, 4,
        "one typed allocation per native callee: {delta:?}"
    );
    assert_eq!(delta.alloc_value_stub_miss, 0, "{delta:?}");
    assert_eq!(delta.alloc_value_stub_out_of_memory, 0, "{delta:?}");
    assert_eq!(delta.alloc_value_stub_other, 0, "{delta:?}");
}

#[test]
fn invalid_length_throws_once_reuses_graph_caller_and_reconciles_generated_deopts() {
    const MODULE: &str = "jit-graph-only-array-exit.js";
    let mut runtime = runtime();
    let trace = Arc::new(Mutex::new(ArrayProbeTrace::default()));
    runtime.set_tracer(Some(Box::new(ArrayProbeTracer(trace.clone()))));
    let setup = runtime
        .run_script(SourceInput::from_javascript(INVALID_LENGTH_SETUP), MODULE)
        .expect("Graph-only ArrayConstruct setup");
    let names = traced_function_names(
        &trace.lock().unwrap(),
        &["callInvalidArrayLength", "invalidArrayLength"],
    );
    let caller_fid = names[0].1;
    let callee_fid = names[1].1;
    let caller = current_graph(&runtime, caller_fid);
    let callee = current_graph(&runtime, callee_fid);
    assert_eq!(callee.generated_entries, 0);
    assert_eq!(callee.generated_deopts, 0);
    let artifacts = setup.jit_artifacts().unwrap();
    assert!(!artifacts.truncated());
    assert_current_graph_call(artifacts, &caller, &callee);
    let (construct_pc, construct_byte_pc) =
        array_construct_location(artifacts, MODULE, "invalidArrayLength");
    drop(setup);

    trace.lock().unwrap().recording = true;
    let before = runtime.execution_stats();
    let probe = runtime
        .run_script(
            SourceInput::from_javascript(INVALID_LENGTH_PROBE),
            "jit-graph-only-array-exit-probe.js",
        )
        .expect("Graph-only ArrayConstruct probe");
    assert_eq!(
        probe.completion_string(),
        r#"["RangeError","Invalid array length",1,3,2]"#
    );
    let delta = CounterDelta::between(before, runtime.execution_stats());
    let trace = trace.lock().unwrap();
    assert_complete_probe_trace(&trace);
    assert!(!trace.ticks.iter().any(|(fid, _, _, _)| *fid == caller_fid));
    let replay: Vec<_> = trace
        .ticks
        .iter()
        .filter(|(fid, _, _, _)| *fid == callee_fid)
        .collect();
    assert_eq!(
        replay.len(),
        1,
        "only the pre-effect throwing opcode replays; recovery stays native"
    );
    assert_eq!(replay[0].2, construct_byte_pc);
    assert_eq!(replay[0].3, Op::ArrayConstruct);
    drop(trace);
    let report = probe.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    let exits: Vec<_> = report
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::EnteredGenerationDeopt {
                callee_function_id,
                callee_code_object_id,
                callee_tier,
                callee_resume_pc,
                exit_reason,
                exit_action,
            } => Some((
                *callee_function_id,
                *callee_code_object_id,
                *callee_tier,
                *callee_resume_pc,
                *exit_reason,
                *exit_action,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        exits,
        vec![(
            callee_fid,
            callee.code_object_id,
            JitDebugTier::Optimizing,
            construct_pc,
            ExitReason::AllocationMiss,
            ExitAction::Resume
        )]
    );
    assert!(!report.events().iter().any(|event| matches!(
        event,
        JitDebugEvent::Bail { .. }
            | JitDebugEvent::InlineDeoptFrame { .. }
            | JitDebugEvent::CompilePrepared { .. }
    )));
    let after_caller = current_graph(&runtime, caller_fid);
    let after_callee = current_graph(&runtime, callee_fid);
    assert_eq!(after_caller.code_object_id, caller.code_object_id);
    assert_eq!(after_callee.code_object_id, callee.code_object_id);
    assert_eq!(after_caller.active_count, 0);
    assert_eq!(after_callee.active_count, 0);
    assert_eq!(after_caller.generated_entries, 0);
    assert_eq!(after_caller.generated_deopts, caller.generated_deopts);
    assert_eq!(after_callee.generated_entries, 0);
    assert_eq!(after_callee.generated_deopts, callee.generated_deopts + 1);
    assert_eq!(delta.generated_calls, 0, "Graph entries remain uncounted");
    assert_eq!(delta.generated_template_entries, 0);
    assert_eq!(delta.generated_template_returns, 0);
    assert_eq!(delta.generated_template_deopts, 0);
    assert_eq!(delta.generated_call_deopts, 1);
    assert_eq!(delta.generated_optimizing_deopts, 1);
    assert_eq!(delta.optimized_deopts, 1);
    assert_eq!(delta.code_generations, 0);
    assert_eq!(delta.optimized_osr_entries, 0);
    assert_eq!(delta.to_rust_call_transitions, 0);
    assert_eq!(delta.alloc_value_stub_ok, 1);
    assert_eq!(delta.alloc_value_stub_miss, 1);
    assert_eq!(delta.alloc_value_stub_out_of_memory, 0);
    assert_eq!(delta.alloc_value_stub_other, 0);
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
    assert_direct_edge(
        artifacts,
        MODULE,
        "constructOrdinaryArrayField",
        "OrdinaryArrayField",
    );
    assert_graph_entry(artifacts, MODULE, "OrdinaryArrayField");
    drop(setup);

    let before = runtime.execution_stats();
    let completion = run(
        &mut runtime,
        ORDINARY_CONSTRUCTOR_PROBE,
        "jit-stack-owned-array-ordinary-constructor-probe.js",
    );
    let delta = CounterDelta::between(before, runtime.execution_stats());
    assert_eq!(completion, "[0,0]");
    // The callee may run in either native tier; no call leaves native code.
    assert_no_exits(delta);
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
    assert_direct_edge(
        artifacts,
        MODULE,
        "constructNonExtensibleOrdinary",
        "NonExtensibleOrdinaryField",
    );
    assert_graph_constructor_fields(artifacts, MODULE, "NonExtensibleOrdinaryField", 1);
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
    assert_direct_edge(
        artifacts,
        MODULE,
        "constructWideOrdinary",
        "WideOrdinaryFields",
    );
    assert_graph_constructor_fields(artifacts, MODULE, "WideOrdinaryFields", 4);
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
    // The callee may run in either native tier; no call leaves native code.
    assert_no_exits(delta);
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
fn tiny_base_constructor_uses_only_template_or_the_graph_optimizer() {
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
            bundle.manifest().tier() != JitDebugTier::Optimizing || is_graph_bundle(bundle)
        }),
        "every optimizing artifact must come from the graph pipeline"
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
checksum;
"#
    );
    let expected_checksum =
        u64::from(iterations) * u64::from(iterations - 1) / 2 + u64::from(iterations) * 4;
    let before = runtime.execution_stats();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(probe),
            "jit-stack-owned-array-gc-probe.js",
        )
        .expect("ArrayConstruct GC probe");
    let delta = CounterDelta::between(before, runtime.execution_stats());

    assert_eq!(result.completion_string(), expected_checksum.to_string());
    // The probe ends on the plain `checksum` completion: an unprofiled call
    // after the OSR-compiled loop would soft-deopt its first execution, which
    // says nothing about the ArrayConstruct edges this test covers. Once the
    // probe loop enters OSR code, its callers and their ArrayConstruct callees
    // run inside it or in graph generations, which count no entries; every
    // Array still allocates through the generated allocation stub.
    assert_eq!(
        delta.generated_template_entries, delta.generated_template_returns,
        "{delta:?}"
    );
    assert_no_exits_except_checksum_overflow(&result, delta);
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
        .expect("completed ArrayConstruct calls must leave only live call-site roots");
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
    assert_eq!(
        reuse_delta.generated_template_entries, reuse_delta.generated_template_returns,
        "{reuse_delta:?}"
    );
    assert_no_exits(reuse_delta);
    assert_eq!(reuse_delta.code_generations, 0, "{reuse_delta:?}");
    assert!(reuse_delta.alloc_value_stub_ok >= 4, "{reuse_delta:?}");
}

/// The only exit a GC probe may take is the script body's checksum add
/// leaving int32 once; no ArrayConstruct callee or caller exits.
fn assert_no_exits_except_checksum_overflow(
    result: &otter_runtime::ExecutionResult,
    delta: CounterDelta,
) {
    let exits = result
        .jit_debug_report()
        .expect("events enabled")
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::Bail {
                function_name,
                exit_reason,
                ..
            } => Some((function_name.clone(), *exit_reason)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        exits.len() <= 1
            && exits.iter().all(|(name, reason)| name == "<main>"
                && *reason == otter_vm::native_abi::ExitReason::Int32Overflow),
        "{exits:?}"
    );
    assert_eq!(delta.optimized_deopts, exits.len() as u64, "{delta:?}");
    assert_eq!(delta.generated_call_deopts, 0, "{delta:?}");
    assert_eq!(delta.generated_deopts(), 0, "{delta:?}");
    assert_eq!(delta.to_rust_call_transitions, 0, "{delta:?}");
}
