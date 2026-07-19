//! Runtime-boundary coverage for owned, default-off JIT artifact bundles.
//!
//! # Contents
//! - Template and optimizing OSR bundle contents.
//! - Template method-inline guard/body/deopt subregions and compact scratch
//!   layout metadata.
//! - Abrupt completion followed by explicit bundle draining.
//! - Bundle ownership across full GC, later allocation, and nested JIT entry.
//! - Async [`otter_runtime::Otter`] success and abrupt-failure transport.
//!
//! # Invariants
//! - Artifact capture is independent from structured event capture.
//! - Every retained payload owns its bytes and contains no VM or GC handle.
//! - Exact code and every native offset in its code map agree on one finalized
//!   executable mapping.
//! - Annotated assembly is UTF-8, offset-based, and never exposes resolved
//!   process-local relocation values.
//!
//! # See also
//! - `otter_vm::jit_artifact` for the current manifest and bounded batch.

#![cfg(target_arch = "aarch64")]

use std::collections::BTreeSet;

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTarget,
    JitDebugTier, JitSelection, Otter, Runtime, SourceInput,
};

const HOT_LOOP: &str = r#"
function hot(limit) {
  let total = 0;
  for (let i = 0; i < limit; i++) {
    total += i;
  }
  return total;
}
hot(96);
"#;

const NESTED_JIT: &str = r#"
function inner(limit) {
  let total = 0;
  for (let i = 0; i < limit; i++) total += i;
  return total;
}

function outer(limit) {
  let total = 0;
  for (let i = 0; i < 12; i++) total += inner(limit + i);
  return total;
}

outer(48);
"#;

const OPTIMIZING_OSR: &str = r#"
function onceOverflow(limit) {
  let index = 0;
  let total = 2147483000;
  while (index < limit) {
    total = total + 100;
    index = index + 1;
  }
  return total;
}
String(onceOverflow(20));
"#;

const OPTIMIZING_MATH: &str = r#"
function sumAbsoluteOffsets(limit) {
  let total = 0;
  for (let index = 0; index < limit; index++) {
    total += Math.abs(index - 32);
  }
  return total;
}
String(sumAbsoluteOffsets(96));
"#;

const TEMPLATE_INLINE_METHOD: &str = r#"
function apply(value) {
  return value + this.bias;
}

function callMethod(receiver, value) {
  return receiver.apply(value);
}

const receiver = { bias: 4, apply };
for (let i = 0; i < 5000; i++) {
  callMethod(receiver, i);
}
JSON.stringify(callMethod(receiver, 9));
"#;

fn runtime_with_artifacts(selection: JitSelection, threshold: u32) -> Runtime {
    Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(threshold)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("runtime with JIT artifacts")
}

fn code_map(bundle: &JitArtifactBundle) -> serde_json::Value {
    serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::CodeMap)
            .expect("code-map payload")
            .contents(),
    )
    .expect("valid code-map JSON")
}

fn assert_assembly_is_symbolic(bundle: &JitArtifactBundle) {
    let assembly = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::Assembly)
            .expect("annotated assembly payload")
            .contents(),
    )
    .expect("annotated assembly is UTF-8");
    let mut lines = assembly.lines();
    assert_eq!(
        lines.next(),
        Some("; otter jit aarch64 assembly"),
        "current assembly header"
    );
    assert_eq!(
        lines.next(),
        Some("; offset-basis=code.bin"),
        "assembly offsets use exact code.bin"
    );
    assert!(
        assembly.lines().any(|line| {
            line.strip_prefix('L')
                .and_then(|label| label.strip_suffix(':'))
                .is_some_and(|offset| {
                    offset.len() == 8 && offset.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        }),
        "branch targets must have stable offset labels:\n{assembly}"
    );
    assert!(
        assembly.lines().any(|line| {
            line.strip_prefix("+0x")
                .and_then(|instruction| instruction.split_once(':'))
                .is_some_and(|(offset, _)| {
                    offset.len() == 8 && offset.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        }),
        "instructions must carry exact code.bin offsets:\n{assembly}"
    );
    assert!(
        assembly.contains("pc=") && assembly.contains("tier-op="),
        "assembly must retain bytecode/IR correlation annotations:\n{assembly}"
    );
    let relocation_lines = assembly
        .lines()
        .filter(|line| line.contains("relocation "))
        .collect::<Vec<_>>();
    assert!(
        !relocation_lines.is_empty(),
        "baked process values must render as symbolic relocations:\n{assembly}"
    );
    for line in relocation_lines {
        let (_, rendered) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("relocation lacks an exact offset: {line}"));
        assert!(
            rendered.trim_start().starts_with("relocation ") && !rendered.contains("0x"),
            "relocation must replace address-bearing instructions with a symbol: {line}"
        );
    }
    assert!(
        !assembly.contains("resolvedValue") && !assembly.contains("0xdead"),
        "assembly must not expose resolved relocation values:\n{assembly}"
    );
}

fn assert_bundle_is_self_consistent(bundle: &JitArtifactBundle) {
    let manifest = bundle.manifest();
    let code = bundle
        .file(JitArtifactFileName::Code)
        .expect("exact code payload");
    let normalized = bundle
        .file(JitArtifactFileName::NormalizedCode)
        .expect("portable normalized code payload");
    let relocations_file = bundle
        .file(JitArtifactFileName::Relocations)
        .expect("typed relocation payload");
    assert_eq!(code.contents().len() as u64, manifest.code_bytes());
    assert!(
        normalized.contents().starts_with(b"OTJNCODE"),
        "normalized code must carry its binary type marker"
    );
    assert!(manifest.bytecode_bytes() > 0);
    assert!(!manifest.function_name().is_empty());
    assert!(!manifest.module().is_empty());
    assert!(!manifest.target().is_empty());
    assert_eq!(manifest.architecture(), std::env::consts::ARCH);
    assert_eq!(manifest.operating_system(), std::env::consts::OS);

    let relocations: serde_json::Value =
        serde_json::from_slice(relocations_file.contents()).expect("valid relocation JSON");
    assert_eq!(relocations["offsetBasis"], "code.bin");
    assert!(
        !String::from_utf8_lossy(relocations_file.contents()).contains("resolvedValue"),
        "relocation payload must not expose resolved process addresses"
    );
    let mut previous_end = 0;
    for relocation in relocations["relocations"]
        .as_array()
        .expect("relocation records")
    {
        let start = relocation["startOffset"]
            .as_u64()
            .expect("relocation start");
        let end = relocation["endOffset"].as_u64().expect("relocation end");
        assert!(
            previous_end <= start,
            "overlapping relocations: {relocation}"
        );
        assert!(start < end, "empty relocation: {relocation}");
        assert!(
            end <= manifest.code_bytes(),
            "relocation escapes code.bin: {relocation}"
        );
        previous_end = end;
    }

    let map = code_map(bundle);
    let code_len = manifest.code_bytes();
    assert!(
        map["entryOffset"]
            .as_u64()
            .is_some_and(|offset| offset < code_len)
    );
    for region in map["regions"].as_array().expect("code-map regions") {
        let start = region["startOffset"].as_u64().expect("region start");
        let end = region["endOffset"].as_u64().expect("region end");
        assert!(start <= end, "invalid code-map range: {region}");
        assert!(end <= code_len, "code-map range escapes code.bin: {region}");
    }
    for entry in map["osrEntries"].as_array().expect("OSR entries") {
        let start = entry["startOffset"].as_u64().expect("OSR start");
        let end = entry["endOffset"].as_u64().expect("OSR end");
        assert!(start < end, "empty OSR entry: {entry}");
        assert!(end <= code_len, "OSR entry escapes code.bin: {entry}");
    }
    assert_assembly_is_symbolic(bundle);
}

fn first_tier_bundle(batch: &JitArtifactBatch, tier: JitDebugTier) -> &JitArtifactBundle {
    batch
        .bundles()
        .iter()
        .find(|bundle| bundle.manifest().tier() == tier)
        .unwrap_or_else(|| panic!("missing {tier:?} bundle in {batch:?}"))
}

fn bundle_relocation_target_kinds(bundle: &JitArtifactBundle) -> BTreeSet<String> {
    let document: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::Relocations)
            .expect("relocation payload")
            .contents(),
    )
    .expect("valid relocation JSON");
    document["relocations"]
        .as_array()
        .expect("relocation records")
        .iter()
        .map(|relocation| {
            relocation["target"]["kind"]
                .as_str()
                .expect("typed relocation kind")
                .to_string()
        })
        .collect()
}

fn relocation_target_kinds(batch: &JitArtifactBatch) -> BTreeSet<String> {
    batch
        .bundles()
        .iter()
        .flat_map(bundle_relocation_target_kinds)
        .collect()
}

#[test]
fn template_osr_returns_a_current_owned_bundle_without_events() {
    let mut runtime = runtime_with_artifacts(JitSelection::Template, 1);
    let result = runtime
        .run_script(
            SourceInput::from_javascript(HOT_LOOP),
            "jit-artifact-template.js",
        )
        .expect("template OSR fixture");
    let batch = result.jit_artifacts().expect("enabled artifact batch");
    let bundle = first_tier_bundle(batch, JitDebugTier::Template);

    assert_eq!(result.completion_string(), "4560");
    assert!(result.jit_debug_report().is_none());
    assert_eq!(bundle.manifest().module(), "jit-artifact-template.js");
    assert!(matches!(
        bundle.manifest().entry(),
        JitDebugTarget::Osr { .. }
    ));
    assert!(bundle.file(JitArtifactFileName::Bytecode).is_some());
    assert!(bundle.file(JitArtifactFileName::TemplatePlan).is_some());
    assert!(bundle.file(JitArtifactFileName::OptimizedIr).is_none());
    assert!(bundle.file(JitArtifactFileName::Safepoints).is_some());
    assert!(bundle.file(JitArtifactFileName::Relocations).is_some());
    assert!(bundle.file(JitArtifactFileName::NormalizedCode).is_some());
    assert!(bundle.file(JitArtifactFileName::Assembly).is_some());
    assert_bundle_is_self_consistent(bundle);
}

#[test]
fn template_method_inline_artifact_exposes_compact_scratch_and_deopt_ranges() {
    let mut runtime = runtime_with_artifacts(JitSelection::ProductionTiered, u32::MAX);
    let result = runtime
        .run_script(
            SourceInput::from_javascript(TEMPLATE_INLINE_METHOD),
            "jit-artifact-inline-method.js",
        )
        .expect("template inline-method artifact fixture");
    let batch = result.jit_artifacts().expect("enabled artifact batch");
    let (bundle, map) = batch
        .bundles()
        .iter()
        .filter(|bundle| bundle.manifest().tier() == JitDebugTier::Template)
        .find_map(|bundle| {
            let map = code_map(bundle);
            map["regions"]
                .as_array()
                .is_some_and(|regions| {
                    regions
                        .iter()
                        .any(|region| region["kind"] == "inlineScratchSetup")
                })
                .then_some((bundle, map))
        })
        .expect("template method-inline bundle");
    let regions = map["regions"].as_array().expect("code-map regions");
    let kinds = regions
        .iter()
        .filter_map(|region| region["kind"].as_str())
        .collect::<BTreeSet<_>>();

    assert_eq!(result.completion_string(), "13");
    for expected in [
        "inlineMethodGuard",
        "inlineScratchSetup",
        "inlineInstruction",
        "inlineMethodBody",
        "inlineMethodHitEpilogue",
        "inlineMethodDeoptTeardown",
    ] {
        assert!(
            kinds.contains(expected),
            "missing inline region {expected}: {kinds:?}"
        );
    }

    let scratch = regions
        .iter()
        .find(|region| region["kind"] == "inlineScratchSetup")
        .expect("inline scratch setup");
    let layout = &scratch["inlineScratchLayout"];
    assert_eq!(layout["parameterCount"], 1);
    assert_eq!(layout["virtualRegisterCount"], 6);
    assert_eq!(layout["scratchSlotCount"], 2);
    assert_eq!(layout["slotBytes"], 8);
    assert_eq!(layout["stackAlignmentBytes"], 16);
    assert_eq!(layout["scratchBytes"], 16);
    assert_eq!(layout["offsetBasis"], "postAllocationSp");
    let virtual_register_count = layout["virtualRegisterCount"]
        .as_u64()
        .expect("virtual register count");
    let scratch_slot_count = layout["scratchSlotCount"]
        .as_u64()
        .expect("scratch slot count");
    let slot_bytes = layout["slotBytes"].as_u64().expect("scratch slot bytes");
    let stack_alignment = layout["stackAlignmentBytes"]
        .as_u64()
        .expect("scratch stack alignment");
    let expected_scratch_bytes =
        (scratch_slot_count * slot_bytes).div_ceil(stack_alignment) * stack_alignment;
    assert_eq!(
        layout["scratchBytes"].as_u64(),
        Some(expected_scratch_bytes)
    );
    assert!(
        scratch_slot_count < virtual_register_count + 1,
        "layout must compact callee registers plus receiver: {layout}"
    );
    let register_slots = layout["registerSlots"]
        .as_array()
        .expect("virtual register slots");
    assert_eq!(register_slots.len() as u64, virtual_register_count);
    for slot in register_slots.iter().filter_map(serde_json::Value::as_u64) {
        assert!(slot < scratch_slot_count, "register slot out of range");
    }
    assert!(register_slots[5].is_null(), "unused r5 stays unmapped");
    assert_eq!(register_slots[0], register_slots[1]);
    assert_eq!(register_slots[1], register_slots[3]);
    assert_eq!(register_slots[2], register_slots[4]);
    assert_ne!(register_slots[0], register_slots[2]);
    assert_eq!(layout["receiverSlot"], register_slots[2]);
    assert!(
        layout["receiverSlot"]
            .as_u64()
            .is_some_and(|slot| slot < scratch_slot_count),
        "receiver slot out of range"
    );
    let entry_values = layout["entryValues"].as_array().expect("entry values");
    assert_eq!(entry_values.len(), 2);
    assert_eq!(entry_values[0]["kind"], "argument");
    assert_eq!(entry_values[0]["argument"], 0);
    assert_eq!(entry_values[0]["register"], 0);
    assert_eq!(entry_values[1]["kind"], "receiver");
    for entry in entry_values {
        assert!(
            entry["slot"]
                .as_u64()
                .is_some_and(|slot| slot < scratch_slot_count),
            "entry slot out of range: {entry}"
        );
    }

    let inline_site = scratch["inlineSite"].clone();
    assert!(inline_site["callerFunctionId"].is_u64());
    assert!(inline_site["logicalPc"].is_u64());
    assert!(inline_site["bytePc"].is_u64());
    assert_eq!(inline_site["hasReceiverProperty"], true);
    let callee_function_id = scratch["functionId"]
        .as_u64()
        .expect("inline callee function id");
    let inline_regions = regions
        .iter()
        .filter(|region| region.get("inlineSite").is_some())
        .collect::<Vec<_>>();
    assert!(!inline_regions.is_empty());
    assert!(inline_regions.iter().all(|region| {
        region["inlineSite"] == inline_site
            && region["functionId"].as_u64() == Some(callee_function_id)
            && region.get("inlineFrame").is_none()
    }));

    let inline_instructions = regions
        .iter()
        .filter(|region| region["kind"] == "inlineInstruction")
        .collect::<Vec<_>>();
    assert_eq!(inline_instructions.len(), 7);
    assert!(inline_instructions.iter().all(|region| {
        region["functionId"].is_u64()
            && region["logicalPc"].is_u64()
            && region["bytePc"].is_u64()
            && region["operationIndex"].is_u64()
            && region["operation"].is_string()
    }));
    assert_eq!(
        inline_instructions
            .iter()
            .map(|region| region["operationIndex"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        (0..7).collect::<Vec<_>>()
    );

    let find_region = |kind: &str| {
        regions
            .iter()
            .find(|region| region["kind"] == kind)
            .unwrap_or_else(|| panic!("missing {kind} region"))
    };
    let native_range = |region: &serde_json::Value| {
        (
            region["startOffset"].as_u64().expect("region start"),
            region["endOffset"].as_u64().expect("region end"),
        )
    };
    let guard_range = native_range(find_region("inlineMethodGuard"));
    let scratch_range = native_range(scratch);
    let body_range = native_range(find_region("inlineMethodBody"));
    let hit_range = native_range(find_region("inlineMethodHitEpilogue"));
    let teardown_range = native_range(find_region("inlineMethodDeoptTeardown"));
    assert!(guard_range.1 <= scratch_range.0);
    assert!(scratch_range.1 <= body_range.0);
    assert_eq!(body_range.1, hit_range.0);
    assert_eq!(hit_range.1, teardown_range.0);
    for instruction in &inline_instructions {
        let instruction_range = native_range(instruction);
        assert!(
            body_range.0 <= instruction_range.0 && instruction_range.1 <= body_range.1,
            "inline instruction escapes body: {instruction}"
        );
    }

    let method_call = regions
        .iter()
        .find(|region| {
            region["kind"] == "instruction"
                && region["operation"]
                    .as_str()
                    .is_some_and(|operation| operation.contains("MethodCall"))
                && region["logicalPc"] == inline_site["logicalPc"]
                && region["bytePc"] == inline_site["bytePc"]
        })
        .expect("caller MethodCall region");
    let method_call_range = native_range(method_call);
    assert_eq!(
        method_call["functionId"], inline_site["callerFunctionId"],
        "inline site must identify caller code map"
    );
    assert!(
        method_call_range.0 <= guard_range.0 && teardown_range.1 <= method_call_range.1,
        "inline ranges must stay inside caller MethodCall: {method_call}"
    );

    let assembly = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::Assembly)
            .expect("annotated assembly")
            .contents(),
    )
    .expect("annotated assembly is UTF-8");
    assert!(assembly.contains("kind=inlineMethodGuard"));
    assert!(assembly.contains("kind=inlineMethodDeoptTeardown"));
    assert!(!assembly.contains("inlineMethodMissReplay"));
    assert!(assembly.contains("inline-site=caller:"));
    assert!(assembly.contains("receiver-property=true"));
    assert!(assembly.contains(
        "parameters=1 virtual-registers=6 scratch-slots=2 slot-bytes=8 stack-alignment=16 scratch-bytes=16 offset-basis=postAllocationSp"
    ));
    assert!(assembly.contains("register-slots=["));
    assert!(assembly.contains("entry-values=[arg0->r0:s"));
    assert!(assembly.contains(",receiver:s"));
    assert_bundle_is_self_consistent(bundle);
}

#[test]
fn optimizing_osr_returns_ir_deopt_and_safepoint_payloads() {
    let mut runtime = runtime_with_artifacts(JitSelection::ProductionTiered, 4);
    let result = runtime
        .run_script(
            SourceInput::from_javascript(OPTIMIZING_OSR),
            "jit-artifact-optimizing.js",
        )
        .expect("optimizing OSR fixture");
    let batch = result.jit_artifacts().expect("enabled artifact batch");
    let bundle = first_tier_bundle(batch, JitDebugTier::Optimizing);

    assert_eq!(result.completion_string(), "2147485000");
    assert!(matches!(
        bundle.manifest().entry(),
        JitDebugTarget::Osr { .. }
    ));
    assert!(bundle.file(JitArtifactFileName::OptimizedIr).is_some());
    assert!(bundle.file(JitArtifactFileName::TemplatePlan).is_none());
    assert!(bundle.file(JitArtifactFileName::Deopt).is_some());
    assert!(bundle.file(JitArtifactFileName::Safepoints).is_some());
    assert!(bundle.file(JitArtifactFileName::Assembly).is_some());
    let deopt: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::Deopt)
            .expect("deopt payload")
            .contents(),
    )
    .expect("valid deopt JSON");
    assert!(deopt["exits"].is_array());
    let optimized_ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("optimized IR payload")
            .contents(),
    )
    .expect("optimized IR is UTF-8");
    let map = code_map(bundle);
    for region in map["regions"].as_array().expect("code-map regions") {
        if region["kind"] == "instruction" {
            let operation_index = region["operationIndex"]
                .as_u64()
                .expect("optimizer instruction operation index");
            assert!(
                optimized_ir.contains(&format!("op={operation_index:04} ")),
                "code-map operation must join to optimized-ir.txt: {region}"
            );
        }
        if region["kind"] == "blockPrelude" {
            assert!(region["block"].is_u64(), "block prelude identity: {region}");
        }
        if region["kind"] == "fallthroughEdge" {
            assert!(region["block"].is_u64(), "edge source identity: {region}");
            assert!(
                region["targetBlock"].is_u64(),
                "edge target identity: {region}"
            );
        }
    }
    assert_bundle_is_self_consistent(bundle);
}

#[test]
fn optimizing_math_artifact_types_code_owned_argument_slice() {
    let mut runtime = runtime_with_artifacts(JitSelection::ProductionTiered, 4);
    let result = runtime
        .run_script(
            SourceInput::from_javascript(OPTIMIZING_MATH),
            "jit-artifact-optimizing-math.js",
        )
        .expect("optimizing Math fixture");
    let batch = result.jit_artifacts().expect("enabled artifact batch");
    let bundle = first_tier_bundle(batch, JitDebugTier::Optimizing);
    let kinds = bundle_relocation_target_kinds(bundle);

    assert_eq!(result.completion_string(), "2544");
    assert!(
        kinds.contains("optimizedMathArguments"),
        "optimizing Math arguments must be represented symbolically: {kinds:?}"
    );
    assert!(
        kinds.contains("runtimeStub"),
        "optimizing Math call must retain its runtime entry identity: {kinds:?}"
    );
}

#[test]
fn template_artifacts_type_every_non_stub_address_class() {
    let mut runtime = runtime_with_artifacts(JitSelection::Template, 4);
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
const receiver = { value: 3 };
const entries = new Map();
entries.set("key", 5);

function sumMany(a, b, c, d, e, f, g) {
  return a + b + c + d + e + f + g;
}

for (let warm = 0; warm < 64; warm++) {
  sumMany(1, 2, 3, 4, 5, 6, 7);
}

function hot(limit) {
  const bias = 2;
  const addBias = value => value + bias;
  let total = 0;
  for (let i = 0; i < limit; i++) {
    const row = [1, 2, 3];
    total += receiver.value;
    total += row[0];
    total += entries.get("key");
    total += addBias(i);
    total += sumMany(1, 2, 3, 4, 5, 6, 7);
  }
  return total;
}

String(hot(48));
"#,
            ),
            "jit-artifact-target-classes.js",
        )
        .expect("rich template relocation fixture");
    let batch = result.jit_artifacts().expect("enabled artifact batch");
    let kinds = relocation_target_kinds(batch);

    for expected in [
        "runtimeStub",
        "directCallEntryCell",
        "gcCageBase",
        "globalLexicalCell",
        "propertyIcCell",
        "templateOperandSlice",
        "collectionHeapReference",
        "collectionBuiltinFunction",
    ] {
        assert!(
            kinds.contains(expected),
            "missing relocation target {expected}: {kinds:?}"
        );
    }
}

#[test]
fn abrupt_throw_leaves_artifacts_available_for_explicit_drain() {
    let mut runtime = runtime_with_artifacts(JitSelection::Template, 1);
    let source = format!("{HOT_LOOP}\nthrow new Error('after hot loop');");

    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "jit-artifact-abrupt.js",
        )
        .expect_err("fixture must throw after tier-up");

    let batch = runtime
        .take_jit_artifacts()
        .expect("abrupt completion retains enabled artifacts");
    assert!(!batch.is_empty());
    assert_bundle_is_self_consistent(first_tier_bundle(&batch, JitDebugTier::Template));
}

#[test]
fn owned_bundle_survives_full_gc_later_allocation_and_nested_jit() {
    let mut runtime = runtime_with_artifacts(JitSelection::Template, 1);
    let result = runtime
        .run_script(
            SourceInput::from_javascript(NESTED_JIT),
            "jit-artifact-nested.js",
        )
        .expect("nested JIT fixture");
    let batch = result
        .jit_artifacts()
        .expect("nested run owns artifacts")
        .clone();
    let stable_copy = batch.clone();
    let compiled_functions = batch
        .bundles()
        .iter()
        .map(|bundle| bundle.manifest().function_id())
        .collect::<BTreeSet<_>>();

    assert!(
        compiled_functions.len() >= 2,
        "fixture must compile nested and outer functions: {compiled_functions:?}"
    );
    runtime.force_gc().expect("full GC");
    let allocation = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
const allocated = [];
for (let i = 0; i < 256; i++) {
  allocated.push({ index: i, label: "entry-" + i });
}
allocated.length;
"#,
            ),
            "jit-artifact-post-gc.js",
        )
        .expect("allocation after full GC");

    assert_eq!(allocation.completion_string(), "256");
    assert_eq!(batch, stable_copy);
    for bundle in batch.bundles() {
        assert_bundle_is_self_consistent(bundle);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn async_success_and_abrupt_failure_transport_owned_artifacts() {
    let otter = Otter::builder()
        .jit_selection(JitSelection::Template)
        .jit_osr_threshold(1)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("async Otter");
    let result = otter.run_script(HOT_LOOP).await.expect("async hot loop");
    let success = result
        .jit_artifacts()
        .expect("async success carries artifacts");
    assert!(!success.is_empty());

    let source = format!("{HOT_LOOP}\nthrow new Error('after hot loop');");
    let attempt = otter
        .run_script_source_with_diagnostics(
            SourceInput::from_javascript(source),
            "jit-artifact-async-abrupt.js",
        )
        .await;
    assert!(attempt.result().is_err(), "fixture must fail");
    let failure = attempt
        .jit_artifacts()
        .expect("async failure carries partial artifacts");
    assert!(!failure.is_empty());
}
