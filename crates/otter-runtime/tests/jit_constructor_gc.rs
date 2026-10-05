//! Constructor arguments and receivers surviving reentrant moving collection.
//!
//! # Contents
//! - Fixed and spread construction through an allocating proxy prototype read.
//! - Exact Template/Graph caller and constructor generations admitted normally.
//! - Actual callable-entry offsets and current permanent function-cell selection.
//! - Owned source/lease observations bracketing the original allocation loop.
//! - Collection and relocation evidence, followed by retained reuse after full GC.
//!
//! # Invariants
//! - The exact installed native wrapper remains suspended across the collecting
//!   getter; its source resolves to its own emitted constructor return site.
//! - Complete dispatch counts exclude interpreted wrapper and target execution.
//! - Observation performs no JavaScript allocation, collection or fixture assert.
//! - Completion preserves the live argument, prototype and allocation count.
//! - Generated-entry counters describe Template promotion work; ordinary VM
//!   entries and Graph entries cannot be inferred from those counters.
//!
//! # See also
//! - `benchmarks/fixtures/engine/reentrant_allocations.rs` shares the workload.
//! - `support/return_sites.rs` joins actual calls to source-owned root records.
//! - `jit_forward_arguments` covers live forwarded argument windows.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use otter_bytecode::Op;
use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTier,
    JitSelection, Runtime, RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx,
    RuntimeNativeError, RuntimeValue, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{CodeLifetimeState, NativeFrameKind, STUB_JIT_CALL, STUB_JIT_CALL_GENERIC},
};
use serde_json::Value as Json;

#[path = "../../../benchmarks/fixtures/engine/reentrant_allocations.rs"]
mod reentrant_allocations;
use reentrant_allocations::AllocationFixture;
// This fixture consumes the complete table validator; Known-call helpers
// in the shared proof module are used by the direct-call fixtures instead.
#[allow(dead_code)]
#[path = "support/return_sites.rs"]
mod return_sites;

const MODULE: &str = "constructor-gc-setup.js";

#[derive(Default)]
struct DispatchTrace {
    recording: bool,
    total: usize,
    counts: BTreeMap<u32, usize>,
}

struct Tracer(Arc<Mutex<DispatchTrace>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut trace = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if trace.recording {
            trace.total += 1;
            *trace.counts.entry(event.function_id).or_default() += 1;
        }
    }
}

#[derive(Debug)]
struct Observation {
    phase: u8,
    frames: String,
    generations: Vec<JitCodeGenerationSnapshot>,
    minor_cycles: u64,
    minor_updates: u64,
}

fn observer(records: Arc<Mutex<Vec<Result<Observation, String>>>>) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        let records = records.clone();
        realm.install_native_global_call(
            "constructorGcObserve",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    // The hook is reached only in the probe's allocating branch.
                    // No raw value, borrowed source, or context escapes this call.
                    let phase =
                        args.first()
                            .and_then(|value| value.as_number())
                            .and_then(|number| match number.as_f64() {
                                0.0 => Some(0),
                                1.0 => Some(1),
                                _ => None,
                            });
                    let frames = ctx
                        .execution_context()
                        .map(|context| ctx.capture_call_sites_json(context, 0, usize::MAX));
                    let observed = match (phase, frames) {
                        (Some(phase), Some(frames)) => {
                            let interp = ctx.interp_mut();
                            let gc = interp.gc_stats_snapshot();
                            Ok(Observation {
                                phase,
                                frames,
                                generations: interp.jit_code_generation_snapshot(),
                                minor_cycles: gc.minor_gc_cycles,
                                minor_updates: gc.minor_slot_updates,
                            })
                        }
                        _ => Err("constructor observation lacks its phase/context".into()),
                    };
                    records
                        .lock()
                        .map_err(|_| RuntimeNativeError::Error {
                            message: "constructor observation lock poisoned".into(),
                        })?
                        .push(observed);
                    Ok(RuntimeValue::undefined())
                },
            )),
        )
    })
}

struct Proof {
    generation: JitCodeGenerationSnapshot,
    bundle: JitArtifactBundle,
}

fn json(bundle: &JitArtifactBundle, name: JitArtifactFileName) -> Json {
    serde_json::from_slice(bundle.file(name).expect("own artifact file").contents())
        .expect("complete artifact JSON")
}

fn current_proof(
    runtime: &Runtime,
    artifacts: &JitArtifactBatch,
    name: &str,
    kind: NativeFrameKind,
    events: &[otter_runtime::JitDebugEvent],
) -> Proof {
    let tier = match kind {
        NativeFrameKind::Baseline => JitDebugTier::Template,
        NativeFrameKind::Optimizing => JitDebugTier::Optimizing,
        _ => panic!("native JavaScript tier"),
    };
    let generations = runtime.jit_code_generation_snapshot();
    let found: Vec<_> = artifacts
        .bundles()
        .iter()
        .filter(|bundle| {
            bundle.manifest().module() == MODULE
                && bundle.manifest().function_name() == name
                && bundle.manifest().tier() == tier
        })
        .filter_map(|bundle| {
            generations
                .iter()
                .find(|generation| {
                    generation.function_id == bundle.manifest().function_id()
                        && generation.code_object_id == bundle.manifest().code_object_id()
                        && generation.tier == kind
                        && generation.lifecycle == CodeLifetimeState::Installed
                        && generation.linked
                        && generation.current_entry
                        && generation.call_entry_offset.is_some()
                })
                .map(|generation| Proof {
                    generation: generation.clone(),
                    bundle: bundle.clone(),
                })
        })
        .collect();
    // The capture label records the trigger. Actual callable capability and
    // current function-cell selection above determine the ordinary entry owner.
    // Bounded failure evidence retains the trigger without broadening the tier.
    let manifests: Vec<_> = artifacts
        .bundles()
        .iter()
        .take(32)
        .map(|bundle| {
            let manifest = bundle.manifest();
            (
                manifest.module(),
                manifest.function_name(),
                manifest.function_id(),
                manifest.code_object_id(),
                manifest.tier(),
                manifest.entry(),
            )
        })
        .collect();
    let relevant: Vec<_> = events
        .iter()
        .filter_map(|event| {
            let encoded = serde_json::to_value(event).expect("owned debug event serializes");
            (encoded["functionName"] == name).then_some(encoded)
        })
        .take(32)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "one own {name} {kind:?} entry; manifests={manifests:?}; events={relevant:?}; generations={generations:?}"
    );
    let proof = found.into_iter().next().unwrap();
    assert_callable_entry(&proof);
    proof
}

fn assert_callable_entry(proof: &Proof) {
    let map = json(&proof.bundle, JitArtifactFileName::CodeMap);
    let offset = map["callEntryOffset"]
        .as_u64()
        .expect("the own compiler emitted a private-JS call entry");
    assert_eq!(
        Some(offset),
        proof.generation.call_entry_offset.map(u64::from),
        "artifact and retained generation name the same actual entry"
    );
    assert!(proof.generation.current_entry && proof.generation.linked);
    let code = proof
        .bundle
        .file(JitArtifactFileName::Code)
        .unwrap()
        .contents();
    assert!(offset < code.len() as u64);
    assert_ne!(map["entryOffset"].as_u64(), Some(offset));
    // Both canonical frame emitters return the offset of their actual save
    // prologue. These are bytes from this exact captured mapping, not a label.
    #[cfg(target_arch = "aarch64")]
    assert_eq!(
        code.get(offset as usize..offset as usize + 4),
        Some(0xa9bd_7bfdu32.to_le_bytes().as_slice())
    );
    #[cfg(target_arch = "x86_64")]
    assert_eq!(
        code.get(offset as usize..offset as usize + 4),
        Some([0x55, 0x48, 0x89, 0xe5].as_slice())
    );
}

fn canonical_instruction(bundle: &JitArtifactBundle, opcode: Op) -> (u32, u32) {
    let opcode = format!("{opcode:?}");
    let bytecode = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::Bytecode)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert_eq!(bytecode.lines().next(), Some("; otter bytecode"));
    assert!(
        bytecode
            .lines()
            .nth(1)
            .unwrap()
            .starts_with(&format!("; function={} ", bundle.manifest().function_id()))
    );
    let canonical: Vec<_> = bytecode
        .lines()
        .skip(2)
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let logical = words.next()?.parse::<u32>().ok()?;
            let encoded = words.next()?.strip_prefix("byte=")?.parse::<u32>().ok()?;
            (words.next()? == opcode.as_str()).then_some((logical, encoded))
        })
        .collect();
    assert_eq!(canonical.len(), 1, "one own {opcode} opcode");
    canonical[0]
}

/// The wrapper has one canonical construct opcode and one emitted region for it.
/// Fixed construction enters the private-JS trampoline; staged spread enters
/// the platform C trampoline. Each actual return names its own source/root record.
fn assert_construct_call(proof: &Proof, opcode: Op) {
    let bundle = &proof.bundle;
    let (pc, byte_pc) = canonical_instruction(bundle, opcode);
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions: Vec<_> = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["functionId"].as_u64() == Some(u64::from(proof.generation.function_id))
                && region["logicalPc"].as_u64() == Some(u64::from(pc))
                && region["bytePc"].as_u64() == Some(u64::from(byte_pc))
                && region["operation"].as_str().is_some_and(|operation| {
                    operation.contains(" Generic {")
                        || operation.contains(" CallJs {")
                        || (opcode == Op::New
                            && operation.starts_with("Construct {")
                            && operation.contains("super_construct: false"))
                        || (opcode == Op::NewSpread
                            && operation
                                .starts_with(&format!("SpreadCallOp {{ opcode: {},", opcode as u8)))
                })
        })
        .collect();
    assert_eq!(regions.len(), 1, "one own emitted constructor region");
    let region = regions[0];
    let start = region["startOffset"].as_u64().unwrap();
    let end = region["endOffset"].as_u64().unwrap();
    assert!(start < end);
    let stub = match opcode {
        Op::New => STUB_JIT_CALL_GENERIC,
        Op::NewSpread => STUB_JIT_CALL,
        _ => panic!("exact fixture constructor opcode"),
    };
    let expected_links = if opcode == Op::NewSpread { 2 } else { 1 };
    // Staged spread enters the existing C trampoline; its continuation sibling
    // enters that same owner. Fixed construction uses the private JS convention.
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let links: Vec<_> = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|link| {
            link["target"]["kind"] == "runtimeStub"
                && link["target"]["id"].as_u64() == Some(u64::from(stub.id))
                && link["target"]["name"] == otter_vm::native_abi::runtime_stub_name(stub.id)
                && link["target"]["signature"]
                    == if opcode == Op::NewSpread {
                        "executionEntry0"
                    } else {
                        "jsCall"
                    }
                && link["startOffset"]
                    .as_u64()
                    .is_some_and(|offset| start <= offset && offset < end)
        })
        .collect();
    assert_eq!(
        links.len(),
        expected_links,
        "own construct and its exact declared continuation entries"
    );
    let primary = links
        .iter()
        .min_by_key(|link| link["startOffset"].as_u64().unwrap())
        .unwrap();
    let link_end = primary["endOffset"].as_u64().unwrap() as usize;
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    #[cfg(target_arch = "aarch64")]
    let call: &[u8] = &[0x00, 0x02, 0x3f, 0xd6];
    #[cfg(target_arch = "aarch64")]
    let return_offset = {
        assert_eq!(code.get(link_end..link_end + call.len()), Some(call));
        link_end + call.len()
    };
    #[cfg(target_arch = "x86_64")]
    let return_offset = {
        use yaxpeax_arch::LengthedInstruction;
        use yaxpeax_x86::amd64::{InstDecoder, Opcode};
        let decoder = InstDecoder::default();
        let mut cursor = link_end;
        loop {
            let instruction = decoder.decode_slice(&code[cursor..end as usize]).unwrap();
            let next = cursor + instruction.len().to_const() as usize;
            assert!(next > cursor && next <= end as usize);
            if instruction.opcode() == Opcode::CALL {
                assert_eq!(
                    &code[cursor..next],
                    &[0x41, 0xff, 0xd3],
                    "exact R11 stub call"
                );
                break next;
            }
            // The platform pair-result adapter prepares registers and stack
            // before its CALL; no other callable can intervene.
            assert!(!matches!(
                instruction.opcode(),
                Opcode::JMP | Opcode::RETURN
            ));
            cursor = next;
        }
    };
    assert!(return_offset as u64 <= end);
    let metadata = return_sites::assert_sites(bundle);
    let site = metadata["returnSites"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|site| site["nativeReturnOffset"].as_u64() == Some(return_offset as u64))
        .collect::<Vec<_>>();
    assert_eq!(site.len(), 1, "actual construct return has one owner");
    let record = metadata["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == site[0]["safepointId"])
        .unwrap();
    assert_eq!(record["callPc"].as_u64(), Some(u64::from(pc)));
    assert!(record["inlineFrames"].as_array().unwrap().is_empty());
    assert!(!record["taggedLocations"].as_array().unwrap().is_empty());
}

fn assert_native_target(proof: &Proof) {
    let bundle = &proof.bundle;
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    assert!(!code.is_empty(), "the own constructor has executable bytes");
    let (pc, byte_pc) = canonical_instruction(bundle, Op::StoreProperty);
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let stores = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["functionId"].as_u64() == Some(u64::from(proof.generation.function_id))
                && region["logicalPc"].as_u64() == Some(u64::from(pc))
                && region["bytePc"].as_u64() == Some(u64::from(byte_pc))
                && region["startOffset"].as_u64().unwrap() < region["endOffset"].as_u64().unwrap()
                && region["operation"].as_str().is_some_and(|operation| {
                    operation.contains("Store") || operation.contains(" Generic {")
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stores.len(),
        1,
        "the own native constructor emits its exact marker store"
    );
    assert!(stores[0]["endOffset"].as_u64().unwrap() <= code.len() as u64);
}

fn assert_observation(observed: &Observation, caller: &Proof, source_line: &str, phase: u8) {
    assert_eq!(observed.phase, phase);
    let frames: Json = serde_json::from_str(&observed.frames).expect("complete source snapshot");
    let matching = frames
        .as_array()
        .unwrap()
        .iter()
        .filter(|frame| frame["functionName"] == caller.bundle.manifest().function_name())
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "one own suspended constructor caller: {frames}"
    );
    assert!(
        matching[0]["scriptName"]
            .as_str()
            .unwrap()
            .ends_with(MODULE)
    );
    assert_eq!(matching[0]["sourceLine"], source_line);
    let current = observed
        .generations
        .iter()
        .filter(|generation| {
            generation.function_id == caller.generation.function_id
                && generation.tier == caller.generation.tier
                && generation.lifecycle == CodeLifetimeState::Installed
                && generation.linked
                && generation.current_entry
        })
        .collect::<Vec<_>>();
    assert_eq!(current.len(), 1, "one current suspended native mapping");
    assert_eq!(current[0].code_object_id, caller.generation.code_object_id);
    assert_eq!(
        current[0].call_entry_offset,
        caller.generation.call_entry_offset
    );
    assert_eq!(
        current[0].active_count, 1,
        "the exact own native lease is live"
    );
    assert_eq!(
        current[0].generated_deopts,
        caller.generation.generated_deopts
    );
}

fn stress_stride() -> u32 {
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
        .parse()
        .unwrap_or(1)
}

fn run_at_selection(fixture: AllocationFixture, selection: JitSelection) {
    let stride = stress_stride();
    let allocations = if stride == 0 { 200_000 } else { 2_048 };
    let records = Arc::new(Mutex::new(Vec::new()));
    let trace = Arc::new(Mutex::new(DispatchTrace::default()));
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(observer(records.clone()))
        .build()
        .expect("constructor GC runtime");
    runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(fixture.setup_observed(allocations)),
            MODULE,
        )
        .expect("warm constructor linkage");
    assert!(
        records.lock().unwrap().is_empty(),
        "warmup never entered the probe branch"
    );
    let artifacts = warm.jit_artifacts().expect("own constructor artifacts");
    assert!(!artifacts.truncated());
    let report = warm.jit_debug_report().expect("complete warm metadata");
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    let (caller_name, target_name, opcode, source_line) = match fixture {
        AllocationFixture::Fixed => (
            "constructGc",
            "GcBaseTarget",
            Op::New,
            "  return new Ctor(marker);",
        ),
        AllocationFixture::Spread => (
            "constructSpreadGc",
            "SpreadGcBaseTarget",
            Op::NewSpread,
            "  return new Ctor(...args);",
        ),
    };
    let kind = if selection == JitSelection::Template {
        NativeFrameKind::Baseline
    } else {
        NativeFrameKind::Optimizing
    };
    let caller = current_proof(&runtime, artifacts, caller_name, kind, report.events());
    let target = current_proof(&runtime, artifacts, target_name, kind, report.events());
    assert_construct_call(&caller, opcode);
    assert_native_target(&target);
    if kind == NativeFrameKind::Baseline {
        assert!(
            caller.generation.generated_entries > 0,
            "the actual warmed wrapper used its own generated call entry"
        );
    }
    let before = runtime.execution_stats();
    trace.lock().unwrap().recording = true;
    let result = runtime
        .run_script(
            SourceInput::from_javascript(fixture.probe()),
            "constructor-gc-probe.js",
        )
        .expect("reentrant constructor probe");
    trace.lock().unwrap().recording = false;
    assert_eq!(
        result.completion_string(),
        AllocationFixture::expected_completion(allocations),
        "{} {selection:?}",
        fixture.name()
    );
    let after = runtime.execution_stats();
    let report = result
        .jit_debug_report()
        .expect("complete measured metadata");
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    let minimum_collections = if stride == 0 {
        1
    } else {
        (allocations as u64 / u64::from(stride)).max(1)
    };
    assert!(
        after.gc_minor_cycles - before.gc_minor_cycles >= minimum_collections,
        "prototype read must collect while constructor roots are live"
    );
    assert!(
        after.gc_minor_slot_updates - before.gc_minor_slot_updates >= 2,
        "probe must relocate live references"
    );
    {
        let observed = records.lock().unwrap();
        assert_eq!(
            observed.len(),
            2,
            "the exact allocation interval is bracketed once"
        );
        let before_loop = observed[0]
            .as_ref()
            .expect("owned pre-allocation observation");
        let after_loop = observed[1]
            .as_ref()
            .expect("owned post-allocation observation");
        assert_observation(before_loop, &caller, source_line, 0);
        assert_observation(after_loop, &caller, source_line, 1);
        assert!(
            after_loop.minor_cycles - before_loop.minor_cycles >= minimum_collections,
            "the original allocation loop collects with the own native caller suspended"
        );
        assert!(
            after_loop.minor_updates - before_loop.minor_updates >= 2,
            "the bracketed allocation loop relocates live roots"
        );
    }
    {
        let trace = trace.lock().unwrap();
        assert!(
            trace.total > 0,
            "the probe main dispatch is actually observed"
        );
        assert_eq!(
            trace.total,
            trace.counts.values().sum::<usize>(),
            "complete probe dispatch counts"
        );
        let current = runtime.jit_code_generation_snapshot();
        for proof in [&caller, &target] {
            assert_eq!(
                trace
                    .counts
                    .get(&proof.generation.function_id)
                    .copied()
                    .unwrap_or(0),
                0,
                "the exact own {} remains native: {:?}",
                proof.bundle.manifest().function_name(),
                trace.counts
            );
            let live = current
                .iter()
                .filter(|entry| {
                    entry.function_id == proof.generation.function_id
                        && entry.tier == kind
                        && entry.lifecycle == CodeLifetimeState::Installed
                        && entry.linked
                        && entry.current_entry
                })
                .collect::<Vec<_>>();
            assert_eq!(
                live.len(),
                1,
                "one current own generation after collection: {current:?}"
            );
            assert_eq!(live[0].code_object_id, proof.generation.code_object_id);
            assert_eq!(
                live[0].call_entry_offset,
                proof.generation.call_entry_offset
            );
            assert_eq!(live[0].generated_deopts, proof.generation.generated_deopts);
            assert_eq!(
                live[0].active_count, 0,
                "native lease released on completion"
            );
        }
    }
    // Graph does not count entries; Proxy's VM-entered target also lies outside
    // generated-call promotion counts. Actual mapping/lease/source and complete
    // dispatch evidence above proves execution independently of that policy.
    runtime.force_gc().expect("completed constructor frames");
    let retained = runtime
        .run_script(
            SourceInput::from_javascript("result.marker;"),
            "constructor-gc-reuse.js",
        )
        .expect("retained argument after full collection");
    assert_eq!(retained.completion_string(), "kept:42");
}

fn run_probe(fixture: AllocationFixture) {
    // The independent interpreter executes the unchanged benchmark fixture;
    // scalar observation must not alter its completion or retained result.
    let allocations = if stress_stride() == 0 { 200_000 } else { 2_048 };
    let mut oracle = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .expect("independent constructor interpreter");
    oracle
        .run_script(
            SourceInput::from_javascript(fixture.setup(allocations)),
            MODULE,
        )
        .expect("original oracle setup");
    let result = oracle
        .run_script(
            SourceInput::from_javascript(fixture.probe()),
            "constructor-gc-probe.js",
        )
        .expect("original oracle probe");
    assert_eq!(
        result.completion_string(),
        AllocationFixture::expected_completion(allocations)
    );
    oracle.force_gc().expect("oracle completed frames");
    let retained = oracle
        .run_script(
            SourceInput::from_javascript("result.marker;"),
            "constructor-gc-reuse.js",
        )
        .expect("oracle retained marker");
    assert_eq!(retained.completion_string(), "kept:42");
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        run_at_selection(fixture, selection);
    }
}

#[test]
fn construct_receiver_and_arguments_survive_reentrant_moving_gc() {
    run_probe(AllocationFixture::Fixed);
}

#[test]
fn spread_array_survives_receiver_preparation_moving_gc() {
    run_probe(AllocationFixture::Spread);
}
