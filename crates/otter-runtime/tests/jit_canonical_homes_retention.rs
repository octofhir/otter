//! Active optimizing-frame retention at a dead canonical tagged home.
//!
//! # Contents
//! - Small and large final-iteration payloads created after actual OSR.
//! - Full-GC samples while the payload is live, dead, and after return.
//! - Exact graph and deopt artifacts separating homes from register windows.
//!
//! # Invariants
//! - Native builders use handle scopes and observation captures own scalars.
//! - The probe's only JavaScript loop belongs to the once-called subject.
//! - Before OSR every payload's array has eight elements; the large payload cannot
//!   have been left in the interpreter's earlier register window.
//! - The subject contains no Generic or LoadWindow operation and cannot exit
//!   its optimizing loop before both active-frame samples finish.
//! - The live Instanceof's exception-rebuild recipe maps its exact payload
//!   SSA value to the tagged home stored before the collecting call.
//! - The measured dense-storage difference is live at the first sample and
//!   released at the dead sample before the optimizing frame returns.
//! - Full and minor pause totals remain separate; no timing threshold is set.
//!
//! # See also
//! - `jit_canonical_homes_gc` covers floating pressure and moving tagged roots.
//! - `otter_jit::graph::metadata` publishes one exact root record per boundary.
//! - `otter-benchmark` memory workloads observe interpreter/idle boundaries.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

#[cfg(target_arch = "x86_64")]
#[path = "support/native_code.rs"]
mod native_code;

#[cfg(target_arch = "x86_64")]
use yaxpeax_x86::amd64::{Opcode, Operand, RegSpec};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitDebugTarget,
    JitDebugTier, JitSelection, Runtime, RuntimeExtensionInstaller, RuntimeNativeCall,
    RuntimeNativeCtx, RuntimeNativeError, RuntimeValue, SourceInput,
};
use otter_vm::native_abi::ExitReason;
use serde::Serialize;
use serde_json::{Value as Json, json};

const FUNCTION: &str = "retentionOnce";
const MODULE: &str = "canonical-retention-setup.js";
const ITERATIONS: usize = 32_768;
const SMALL: usize = 8;
const LARGE: usize = 1 << 20;
const PROBE: &str = "JSON.stringify([retentionOnce(makePayload, target, settings, 32768), calls]);";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Sample {
    phase: &'static str,
    live_bytes: usize,
    allocated_bytes: usize,
    new_allocated_bytes: usize,
    old_allocated_bytes: usize,
    reserved_bytes: u64,
    tracked_bytes: u64,
    page_count: usize,
    full_cycles: u64,
    minor_cycles: u64,
    full_pause_ns_total: u64,
    minor_pause_ns_total: u64,
    last_full_reclaimed_bytes: usize,
    total_full_reclaimed_bytes: usize,
    minor_root_slots_scanned: u64,
    minor_slot_updates: u64,
    optimized_osr_entries: u64,
    live_types: Vec<LiveType>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveType {
    type_tag: usize,
    objects: u64,
    bytes: usize,
}

fn live_types(gc: &otter_gc::GcStats) -> Vec<LiveType> {
    gc.by_type
        .iter()
        .enumerate()
        .filter(|(_, row)| row.live_bytes != 0)
        .map(|(type_tag, row)| LiveType {
            type_tag,
            objects: row.alloc_count_total - row.free_count_total,
            bytes: row.live_bytes,
        })
        .collect()
}

fn sample(interp: &mut otter_vm::Interpreter, phase: &'static str) -> Sample {
    let gc = interp.gc_stats_snapshot();
    let heap = interp.gc_heap().stats();
    Sample {
        phase,
        live_bytes: gc.live_bytes,
        allocated_bytes: heap.allocated_bytes,
        new_allocated_bytes: heap.new_allocated_bytes,
        old_allocated_bytes: heap.old_allocated_bytes,
        reserved_bytes: heap.reserved_bytes,
        tracked_bytes: heap.tracked_bytes,
        page_count: heap.page_count,
        full_cycles: gc.gc_cycles,
        minor_cycles: gc.minor_gc_cycles,
        full_pause_ns_total: gc.full_pause_ns_total,
        minor_pause_ns_total: gc.minor_pause_ns_total,
        last_full_reclaimed_bytes: gc.last_gc_reclaimed_bytes,
        total_full_reclaimed_bytes: heap.total_full_reclaimed,
        minor_root_slots_scanned: gc.minor_root_slots_scanned,
        minor_slot_updates: gc.minor_slot_updates,
        optimized_osr_entries: interp.jit_runtime_stats().optimized_osr_entries,
        live_types: live_types(&gc),
    }
}

fn setup_source(last_length: usize) -> String {
    format!(
        r#"
const settings = {{length: 8}};
let calls = 0, armed = false;
const target = {{[Symbol.hasInstance](value) {{
    if (armed) gcSample(value === null ? 2 : 1);
    calls++;
    if (calls === {gate}) {{ settings.length = {last_length}; armed = true; }}
    return false;
}}}};
function retentionOnce(makePayload, target, settings, count) {{
    let total = 0;
    for (let index = 0; index < count; index++) {{
        try {{
        let payload = makePayload(settings.length);
        payload instanceof target;
        total += payload.marker;
        payload = null;
        null instanceof target;
        }} catch (error) {{ throw error; }}
    }}
    return total;
}}
const warmCarrier = new Array(8);
for (let warm = 0; warm < 5000; warm++) {{
    warmCarrier instanceof target;
    null instanceof target;
}}
calls = 0;
"#,
        gate = 2 * (ITERATIONS - 1),
    )
}

fn artifact_json(bundle: &JitArtifactBundle, name: JitArtifactFileName) -> Json {
    serde_json::from_slice(bundle.file(name).expect("artifact file").contents())
        .expect("artifact JSON")
}

/// Parse the engine's eager-state tuples, not JavaScript source.
fn eager_values(body: &str) -> BTreeMap<usize, u32> {
    let Some((_, eager)) = body.split_once(" eager=") else {
        return BTreeMap::new();
    };
    eager
        .split('(')
        .skip(1)
        .map(|tuple| {
            let tuple = tuple.split_once(')').expect("eager tuple").0;
            let (register, value) = tuple.split_once(',').expect("eager pair");
            (
                register.trim().parse().expect("frame register"),
                value.trim().parse().expect("SSA value"),
            )
        })
        .collect()
}

fn assert_artifact(bundle: &JitArtifactBundle) -> u32 {
    let ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("graph IR")
            .contents(),
    )
    .expect("UTF-8 IR");
    assert!(ir.starts_with("; otter graph\n"));
    let nodes: BTreeMap<u32, &str> = ir
        .lines()
        .filter_map(|line| {
            let (id, body) = line.trim().strip_prefix('v')?.split_once(" = ")?;
            Some((id.parse().expect("node id"), body))
        })
        .collect();
    for body in nodes.values() {
        assert!(
            !body.starts_with("Generic ") && !body.starts_with("LoadWindow("),
            "a register-window operation would confound home retention: {body}"
        );
    }
    let map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().expect("code regions");
    let mut points: Vec<_> = regions
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["operation"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(" Instanceof"))
        })
        .collect();
    points.sort_by_key(|point| point["logicalPc"].as_u64().expect("operation PC"));
    assert_eq!(points.len(), 2, "exact live and dead collecting points");
    let live_pc = points[0]["logicalPc"].as_u64().unwrap();
    let dead_pc = points[1]["logicalPc"].as_u64().unwrap();
    let live_node = points[0]["operationIndex"].as_u64().unwrap() as u32;
    let dead_node = points[1]["operationIndex"].as_u64().unwrap() as u32;
    let payload = nodes[&live_node]
        .split_once(" [")
        .expect("Instanceof inputs")
        .1
        .split([',', ']'])
        .next()
        .unwrap()
        .trim()
        .parse::<u32>()
        .expect("payload input SSA");
    assert!(
        nodes[&payload].starts_with("CallJs "),
        "native payload call"
    );
    assert!(
        eager_values(nodes[&live_node])
            .values()
            .any(|&id| id == payload)
    );
    assert!(
        !eager_values(nodes[&dead_node])
            .values()
            .any(|&id| id == payload),
        "the payload must be dead in the second operation's reconstruction state"
    );

    // The try region gives the live Instanceof a real exception-rebuild
    // recipe. Join this node's eager register/value pairs to that exact
    // recipe, rather than to an arithmetic guard after the payload's read.
    let deopt = artifact_json(bundle, JitArtifactFileName::Deopt);
    let states = deopt["frameStates"].as_array().expect("frame states");
    let exit = deopt["exits"]
        .as_array()
        .expect("exits")
        .iter()
        .find(|exit| {
            exit["reason"] == "runtimeTransition"
                && exit["resumePcs"].as_array().and_then(|pcs| pcs.last())
                    == Some(&Json::from(live_pc))
        })
        .expect("live Instanceof's real exception-rebuild recipe");
    let state = states
        .iter()
        .find(|state| state["id"] == exit["frameStateId"])
        .expect("exit frame state");
    let frames = state["frames"].as_array().expect("frames");
    assert_eq!(frames.len(), 1, "the subject has no spliced body");
    assert_eq!(
        frames[0]["functionId"].as_u64(),
        Some(u64::from(bundle.manifest().function_id()))
    );
    // resumePcs and logicalPc are instruction indices; a frame's bytePc
    // must instead match the instruction region's encoded byte offset.
    assert_eq!(frames[0]["bytePc"], points[0]["bytePc"]);
    let mut homes = BTreeSet::new();
    for (register, value) in eager_values(nodes[&live_node]) {
        if value != payload {
            continue;
        }
        // Sparse frames list `[register, slot]` pairs in register order.
        let slot = frames[0]["slots"]
            .as_array()
            .expect("frame slots")
            .iter()
            .find(|pair| pair[0].as_u64() == Some(register as u64))
            .map(|pair| &pair[1])
            .expect("a live payload register has a recipe");
        assert_eq!(slot["representation"], "tagged");
        assert_eq!(slot["locationKind"], "stackSlot");
        homes.insert(
            slot["locationValue"]
                .as_str()
                .unwrap()
                .parse::<u32>()
                .expect("tagged home offset"),
        );
    }
    assert_eq!(homes.len(), 1, "exact payload home: {deopt}");
    let home = homes.into_iter().next().unwrap();
    assert_eq!(home % 8, 0);
    // Each collecting boundary publishes its own record, which roots exactly
    // the tagged homes written for values live there. The live Instanceof's
    // record names the payload home; the dead one's does not. No home is
    // ever cleared.
    let safepoints = artifact_json(bundle, JitArtifactFileName::Safepoints);
    let roots_home = |id: u32| {
        let record = safepoints["records"]
            .as_array()
            .expect("safepoints")
            .iter()
            .find(|point| point["id"].as_u64() == Some(u64::from(id)))
            .expect("published safepoint record");
        record["taggedLocations"]
            .as_array()
            .expect("tagged roots")
            .iter()
            .any(|location| {
                location["kind"] == "spillSlot"
                    && location["index"].as_u64() == Some(u64::from(home / 8))
            })
    };
    let code = bundle
        .file(JitArtifactFileName::Code)
        .expect("native code")
        .contents();
    let published = assert_native_home_lifetimes(code, &points, home);
    for (operation, id) in &published {
        assert_eq!(
            roots_home(*id),
            *operation != u64::from(dead_node),
            "operation {operation} record {id}: the payload home is rooted exactly while live"
        );
    }
    eprintln!(
        "canonical-home-retention-proof {}",
        json!({
            "codeObjectId": bundle.manifest().code_object_id(),
            "functionId": bundle.manifest().function_id(),
            "payloadNode": payload,
            "livePc": live_pc,
            "deadPc": dead_pc,
            "publishedSafepoints": published,
            "taggedHomeOffset": home,
            "payloadLiveEager": eager_values(nodes[&live_node]),
            "payloadDeadEager": eager_values(nodes[&dead_node]),
        })
    );
    regions
        .iter()
        .filter(|region| {
            region["operation"]
                .as_str()
                .is_some_and(|name| name.contains(" JumpLoop("))
        })
        .map(|region| region["logicalPc"].as_u64().unwrap() as u32)
        .max()
        .expect("subject's backedge")
}

#[cfg(target_arch = "aarch64")]
fn assert_native_home_lifetimes(code: &[u8], points: &[&Json], home: u32) -> BTreeMap<u64, u32> {
    let start = points[0]["startOffset"].as_u64().unwrap() as usize;
    let end = points[0]["endOffset"].as_u64().unwrap() as usize;
    // MOVN w16, #imm; STR w16, [x21, #call_site] publishes record `!imm`.
    let stamp = 0xb900_0000
        | (otter_vm::native_abi::NATIVE_FRAME_CALL_SITE_OFFSET / 4) << 10
        | 21 << 5
        | 16;
    let mut published = BTreeMap::new();
    for point in points {
        let start = point["startOffset"].as_u64().unwrap() as usize;
        let end = point["endOffset"].as_u64().unwrap() as usize;
        let words: Vec<_> = code[start..end]
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect();
        let id = words
            .windows(2)
            .find(|pair| pair[0] & 0xffe0_001f == 0x1280_0010 && pair[1] == stamp)
            .map(|pair| !((pair[0] >> 5) & 0xffff))
            .expect("measured Instanceof publishes its own safepoint record");
        published.insert(point["operationIndex"].as_u64().unwrap(), id);
        assert!(
            !words.contains(&(0xf900_03ff | ((home / 8) << 10))),
            "a tagged home is never cleared"
        );
    }
    // STR Xn, [sp, #home] in the actual collecting operation, before its
    // committed runtime call. This value is still needed by the marker read.
    assert!(code[start..end].chunks_exact(4).any(|word| {
        let word = u32::from_le_bytes(word.try_into().unwrap());
        word & 0xffc0_03e0 == 0xf900_03e0 && ((word >> 10) & 0xfff) * 8 == home
    }));
    published
}

#[cfg(target_arch = "x86_64")]
fn assert_native_home_lifetimes(code: &[u8], points: &[&Json], home: u32) -> BTreeMap<u64, u32> {
    let immediate = |operand| match operand {
        Operand::ImmediateI8 { imm } => Some(imm as u32),
        Operand::ImmediateI32 { imm } => Some(imm as u32),
        Operand::ImmediateI64 { imm } => u32::try_from(imm).ok(),
        Operand::ImmediateU32 { imm } => Some(imm),
        _ => None,
    };
    let mut published = BTreeMap::new();
    for (position, point) in points.iter().enumerate() {
        let instructions = native_code::decode(
            code,
            point["startOffset"].as_u64().unwrap() as usize,
            point["endOffset"].as_u64().unwrap() as usize,
        );
        let (stamp, id) = instructions
            .iter()
            .enumerate()
            .find_map(|(index, instruction)| {
                (instruction.opcode() == Opcode::MOV
                    && instruction.mem_size().and_then(|size| size.bytes_size()) == Some(4)
                    && native_code::base_offset(instruction.operand(0))
                        == Some((
                            RegSpec::r14(),
                            otter_vm::native_abi::NATIVE_FRAME_CALL_SITE_OFFSET as i32,
                        )))
                .then(|| immediate(instruction.operand(1)).map(|id| (index, id)))
                .flatten()
            })
            .expect("measured Instanceof publishes its own safepoint record");
        published.insert(point["operationIndex"].as_u64().unwrap(), id);
        assert!(
            instructions.iter().all(|instruction| {
                instruction.opcode() != Opcode::MOV
                    || immediate(instruction.operand(1)) != Some(0)
                    || native_code::base_offset(instruction.operand(0))
                        != Some((RegSpec::rsp(), home as i32))
            }),
            "a tagged home is never cleared"
        );
        if position == 0 {
            assert!(
                instructions[..stamp]
                    .iter()
                    .any(|instruction| instruction.opcode() == Opcode::MOV
                        && native_code::base_offset(instruction.operand(0))
                            == Some((RegSpec::rsp(), home as i32))
                        && matches!(instruction.operand(1), Operand::Register { reg }
                        if reg.class() == RegSpec::rax().class())),
                "actual collecting operation stores exact live payload home"
            );
        }
    }
    published
}

fn measure(last_length: usize) -> Vec<Sample> {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let osr_before_probe = Arc::new(AtomicU64::new(u64::MAX));
    let large_payloads = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .extension_installer(RuntimeExtensionInstaller::new({
            let observations = observations.clone();
            let osr_before_probe = osr_before_probe.clone();
            let large_payloads = large_payloads.clone();
            move |realm| {
                let osr_baseline = osr_before_probe.clone();
                let large_payloads = large_payloads.clone();
                realm.install_native_global_call(
                    "makePayload",
                    1,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              args: &[RuntimeValue],
                              _state: &[RuntimeValue]| {
                            let length = args[0].as_f64().expect("numeric length") as usize;
                            assert!(length == SMALL || length == LARGE);
                            if length == LARGE {
                                assert!(
                                    ctx.interp_mut().jit_runtime_stats().optimized_osr_entries
                                        > osr_baseline.load(Ordering::Relaxed),
                                    "large payload must be allocated after actual subject OSR"
                                );
                                large_payloads.fetch_add(1, Ordering::Relaxed);
                            }
                            ctx.scope(|mut scope| {
                                let payload = scope.object()?;
                                let storage = scope.array(length)?;
                                scope.set(payload, "storage", storage)?;
                                let marker = scope.number(length as f64);
                                scope.set(payload, "marker", marker)?;
                                Ok::<RuntimeValue, RuntimeNativeError>(scope.finish(payload))
                            })
                        },
                    )),
                )?;
                let observations = observations.clone();
                let osr_baseline = osr_before_probe.clone();
                realm.install_native_global_call(
                    "gcSample",
                    1,
                    RuntimeNativeCall::Dynamic(Arc::new(
                        move |ctx: &mut RuntimeNativeCtx<'_>,
                              args: &[RuntimeValue],
                              _state: &[RuntimeValue]| {
                            let phase = match args[0].as_f64().expect("sample phase") as u32 {
                                1 => "live",
                                2 => "dead",
                                other => panic!("unexpected sample phase {other}"),
                            };
                            let interp = ctx.interp_mut();
                            assert!(
                                interp.jit_runtime_stats().optimized_osr_entries
                                    > osr_baseline.load(Ordering::Relaxed)
                            );
                            let cycles = interp.gc_stats_snapshot().gc_cycles;
                            interp.force_gc().expect("active-frame full GC");
                            let observation = sample(interp, phase);
                            assert!(observation.full_cycles > cycles);
                            observations.lock().unwrap().push(observation);
                            Ok(RuntimeValue::undefined())
                        },
                    )),
                )
            }
        }))
        .build()
        .expect("retention runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(setup_source(last_length)),
            MODULE,
        )
        .expect("retention setup");
    osr_before_probe.store(
        runtime.execution_stats().jit_optimized_osr_entries,
        Ordering::Relaxed,
    );
    let result = runtime
        .run_script(
            SourceInput::from_javascript(PROBE),
            "canonical-retention-probe.js",
        )
        .expect("retention probe");
    assert_eq!(
        result.completion_string(),
        format!(
            "[{},{}]",
            SMALL * (ITERATIONS - 1) + last_length,
            2 * ITERATIONS
        )
    );
    assert_eq!(
        large_payloads.load(Ordering::Relaxed),
        usize::from(last_length == LARGE)
    );
    let batch = result.jit_artifacts().expect("probe artifacts");
    let own: Vec<_> = batch
        .bundles()
        .iter()
        .filter(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == MODULE
                && manifest.function_name() == FUNCTION
                && manifest.tier() == JitDebugTier::Optimizing
                && matches!(manifest.entry(), JitDebugTarget::Osr { .. })
        })
        .collect();
    assert!(!own.is_empty(), "actual subject OSR artifact");
    let function_id = own[0].manifest().function_id();
    let last_loop_pc = own
        .iter()
        .map(|bundle| assert_artifact(bundle))
        .max()
        .unwrap();
    let events = result.jit_debug_report().expect("probe events");
    assert!(!events.truncated());
    for event in events.events() {
        let exit = match event {
            JitDebugEvent::Bail {
                function_id: exited,
                tier: JitDebugTier::Optimizing,
                resume_pc,
                exit_reason,
                ..
            } if *exited == function_id => Some((*resume_pc, *exit_reason)),
            JitDebugEvent::EnteredGenerationDeopt {
                callee_function_id,
                callee_tier: JitDebugTier::Optimizing,
                callee_resume_pc,
                exit_reason,
                ..
            } if *callee_function_id == function_id => Some((*callee_resume_pc, *exit_reason)),
            _ => None,
        };
        if let Some((pc, reason)) = exit {
            assert!(
                pc > last_loop_pc,
                "subject left before the samples: {event:?}"
            );
            assert_eq!(reason, ExitReason::InsufficientFeedback, "{event:?}");
        }
    }
    drop(result);
    let mut observations = observations.lock().unwrap().clone();
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].phase, "live");
    assert_eq!(observations[1].phase, "dead");
    let cycles = runtime.heap_stats().gc_cycles;
    runtime.force_gc().expect("post-return full GC");
    assert!(runtime.heap_stats().gc_cycles > cycles);
    let gc = runtime.heap_stats().clone();
    let heap = runtime.heap_accounting_stats();
    observations.push(Sample {
        phase: "returned",
        live_bytes: gc.live_bytes,
        allocated_bytes: heap.allocated_bytes,
        new_allocated_bytes: heap.new_allocated_bytes,
        old_allocated_bytes: heap.old_allocated_bytes,
        reserved_bytes: heap.reserved_bytes,
        tracked_bytes: heap.tracked_bytes,
        page_count: heap.page_count,
        full_cycles: gc.gc_cycles,
        minor_cycles: gc.minor_gc_cycles,
        full_pause_ns_total: gc.full_pause_ns_total,
        minor_pause_ns_total: gc.minor_pause_ns_total,
        last_full_reclaimed_bytes: gc.last_gc_reclaimed_bytes,
        total_full_reclaimed_bytes: heap.total_full_reclaimed,
        minor_root_slots_scanned: gc.minor_root_slots_scanned,
        minor_slot_updates: gc.minor_slot_updates,
        optimized_osr_entries: runtime.execution_stats().jit_optimized_osr_entries,
        live_types: live_types(&gc),
    });
    observations
}

#[test]
fn measure_dead_tagged_home_retention_inside_an_optimizing_frame() {
    let small = measure(SMALL);
    let large = measure(LARGE);
    let differences: Vec<_> = small
        .iter()
        .zip(&large)
        .map(|(small, large)| {
            let mut type_differences = BTreeMap::<usize, i128>::new();
            for row in &small.live_types {
                *type_differences.entry(row.type_tag).or_default() -= row.bytes as i128;
            }
            for row in &large.live_types {
                *type_differences.entry(row.type_tag).or_default() += row.bytes as i128;
            }
            json!({
                "phase": small.phase,
                "liveBytesLargeMinusSmall": large.live_bytes as i128 - small.live_bytes as i128,
                "allocatedBytesLargeMinusSmall": large.allocated_bytes as i128 - small.allocated_bytes as i128,
                "reservedBytesLargeMinusSmall": i128::from(large.reserved_bytes) - i128::from(small.reserved_bytes),
                "liveTypeByteDifferences": type_differences,
            })
        })
        .collect();
    eprintln!(
        "canonical-home-retention {}",
        serde_json::to_string(&json!({"small": small, "large": large, "differences": differences}))
            .unwrap()
    );
    // Before expiration this exact 8,519,608-byte type-63 dense-storage
    // difference survived the dead sample and disappeared only at return.
    // Check the actual owner row as well as whole-heap accounting, with the
    // native clear and live-home protection proven above.
    let dense_bytes = |sample: &Sample| {
        sample
            .live_types
            .iter()
            .filter(|row| row.type_tag == 0x3f)
            .map(|row| row.bytes as i128)
            .sum::<i128>()
    };
    let expected_dense_difference =
        ((LARGE - SMALL) * 8 + (LARGE.div_ceil(64) - SMALL.div_ceil(64)) * 8) as i128;
    assert_eq!(expected_dense_difference, 8_519_608);
    for (index, (small, large)) in small.iter().zip(&large).enumerate() {
        let expected = if index == 0 {
            expected_dense_difference
        } else {
            0
        };
        assert_eq!(
            dense_bytes(large) - dense_bytes(small),
            expected,
            "dense storage must expire at phase {}",
            small.phase
        );
        assert_eq!(
            large.live_bytes as i128 - small.live_bytes as i128,
            expected,
            "the transitive payload must expire at phase {}",
            small.phase
        );
    }
}
