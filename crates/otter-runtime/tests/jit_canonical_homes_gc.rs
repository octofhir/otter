//! Floating spill homes and a young tagged carrier across a collecting operation.
//!
//! # Contents
//! - A once-called nested loop with 31 simultaneous nonconstant double values.
//! - Allocating custom `Symbol.hasInstance` calls after a separate warmup.
//! - Actual optimizing OSR, relocation counters and exact artifact home proofs.
//!
//! # Invariants
//! - Both runtimes execute identical source and compare the complete result.
//! - The probe's only JavaScript loops belong to the measured function.
//! - Double definitions precede `Instanceof`; all 31 are read afterwards.
//! - Artifact checks join that operation's eager state to distinct untagged
//!   stack homes and require native floating save/load instructions there.
//! - The production run cannot leave its optimizing loop before the allocating
//!   phase; cold exits after the nested loops do not invalidate this evidence.
//!
//! # See also
//! - `jit_constructor_gc` proves generated constructor relocation after warmup.
//! - `optimizing_osr` proves optimizing entry and exact post-OSR deopt parity.
//! - `otter-jit/src/graph/regalloc_tests.rs` checks GP and FP pressure plans.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

#[cfg(target_arch = "x86_64")]
#[path = "support/native_code.rs"]
mod native_code;

#[cfg(target_arch = "x86_64")]
use yaxpeax_x86::amd64::{Opcode, Operand, RegSpec};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitDebugTarget,
    JitDebugTier, JitSelection, Runtime, SourceInput,
};
use otter_vm::native_abi::ExitReason;
use serde_json::Value as Json;

const FUNCTION: &str = "fpPressureOnce";
const MODULE: &str = "canonical-homes-setup.js";
const FLOATS: usize = 31;
const ITERATIONS: usize = 8 * 4096;
const PROBE: &str = "JSON.stringify([fpPressureOnce(input, target), calls, allocations]);";

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

/// Generate source templates, without parsing or transforming JavaScript.
fn setup_source(collecting_calls: usize) -> String {
    let mut source = String::from("const input = {\n");
    for index in 0..FLOATS {
        writeln!(source, "p{index}: {index}.5,").unwrap();
    }
    source.push_str("};\nconst sink = new Array(16);\nlet calls = 0;\nlet allocations = 0;\n");
    writeln!(
        source,
        "const allocationGate = {};",
        ITERATIONS - collecting_calls
    )
    .unwrap();
    source.push_str(
        "const target = { [Symbol.hasInstance](value) {\n\
         calls++;\nif (calls > allocationGate) {\n",
    );
    // No helper loop can claim the probe's optimizing OSR counter. Sixteen
    // allocations guarantee a collection inside this operation at every
    // supported stress stride, while its caller's canonical homes are live.
    for index in 0..16 {
        writeln!(source, "sink[{index}] = {{keep: value, padding: calls}};").unwrap();
    }
    source.push_str(
        "allocations += 16;\n}\nreturn value.marker === 7;\n} };\n\
         function fpPressureOnce(input, target) {\n\
         let total = 0.5, hits = 0, links = 0;\n\
         for (let outer = 0; outer < 8; outer++) {\n\
         for (let inner = 0; inner < 4096; inner++) {\n\
         try {\nconst live = {marker: 7, next: input};\n",
    );
    // TryPush runs before these definitions. The collecting operation's
    // exception recipe therefore exposes their homes without a generic
    // operation first flushing the newly defined floating registers.
    for index in 0..FLOATS {
        writeln!(source, "const x{index} = input.p{index} + 0.125;").unwrap();
    }
    source.push_str("const hit = live instanceof target;\ntotal += ");
    for index in 0..FLOATS {
        if index != 0 {
            source.push_str(" + ");
        }
        // Distinct positive weights make a permutation of restored values
        // observable; an unweighted sum could conceal swapped spill homes.
        write!(source, "x{index} * {}", index + 1).unwrap();
    }
    source.push_str(
        ";\ntotal += live.marker;\nhits += hit ? 1 : 0;\n\
         links += live.next === input ? 1 : 0;\n\
         } catch (error) { throw error; }\n}\n}\n\
         return [total, hits, links];\n}\n\
         const warmCarrier = {marker: 7, next: input};\n\
         for (let warm = 0; warm < 5000; warm++) warmCarrier instanceof target;\n\
         calls = 0;\n",
    );
    source
}

fn json(bundle: &JitArtifactBundle, name: JitArtifactFileName) -> Json {
    serde_json::from_slice(bundle.file(name).expect("artifact payload").contents())
        .expect("artifact JSON")
}

fn assert_pressure_artifact(bundle: &JitArtifactBundle) -> u32 {
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().expect("code regions");
    let operation = regions
        .iter()
        .find(|region| {
            region["kind"] == "instruction"
                && region["operation"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(" Instanceof"))
        })
        .expect("the measured collecting Instanceof node");
    let node = operation["operationIndex"].as_u64().expect("node id") as u32;
    let pc = operation["logicalPc"].as_u64().expect("Instanceof PC") as u32;
    let last_loop_pc = regions
        .iter()
        .filter(|region| {
            region["operation"]
                .as_str()
                .is_some_and(|name| name.contains(" JumpLoop("))
        })
        .map(|region| region["logicalPc"].as_u64().expect("loop PC") as u32)
        .max()
        .expect("nested loop backedges");

    let ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("graph IR")
            .contents(),
    )
    .expect("UTF-8 graph IR");
    let nodes: BTreeMap<u32, &str> = ir
        .lines()
        .filter_map(|line| {
            let (id, body) = line.trim().strip_prefix('v')?.split_once(" = ")?;
            Some((id.parse().expect("IR node id"), body))
        })
        .collect();
    let eager = nodes[&node]
        .split_once(" eager=")
        .expect("Instanceof eager frame state")
        .1;
    // This parses the engine's tuple-based IR metadata, not JavaScript.
    let float_values: BTreeMap<usize, u32> = eager
        .split('(')
        .skip(1)
        .map(|tuple| {
            let tuple = tuple.split_once(')').expect("IR state tuple").0;
            let (register, value) = tuple.split_once(',').expect("register and value");
            (
                register.trim().parse().expect("IR frame register"),
                value.trim().parse::<u32>().expect("IR state value"),
            )
        })
        .filter(|(_, id)| {
            nodes
                .get(id)
                .is_some_and(|body| body.starts_with("Float64Add ") && body.ends_with(" Float64"))
        })
        .collect();
    assert!(
        float_values
            .values()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            >= FLOATS,
        "Instanceof must owe 31 nonconstant floating arithmetic values: {eager}",
    );

    let deopt = json(bundle, JitArtifactFileName::Deopt);
    let exit = deopt["exits"]
        .as_array()
        .expect("exit descriptors")
        .iter()
        .find(|exit| {
            exit["reason"] == "runtimeTransition"
                && exit["resumePcs"].as_array().and_then(|pcs| pcs.last()) == Some(&Json::from(pc))
        })
        .expect("Instanceof's real exception-materialization recipe");
    let frame_state = exit["frameStateId"].as_u64().expect("frame-state id");
    let state = deopt["frameStates"]
        .as_array()
        .expect("frame states")
        .iter()
        .find(|state| state["id"].as_u64() == Some(frame_state))
        .expect("Instanceof frame state");
    let frames = state["frames"].as_array().expect("materialized frames");
    assert_eq!(frames.len(), 1, "Instanceof has no spliced JavaScript body");
    assert_eq!(
        frames[0]["functionId"].as_u64(),
        Some(u64::from(bundle.manifest().function_id()))
    );
    let slots = frames[0]["slots"].as_array().expect("frame slots");
    let mut homes_by_value = BTreeMap::new();
    // Join the exact eager (register, SSA value) pairs to their own slots;
    // another live double, such as total, cannot hide a missing xN home.
    for (register, value) in float_values {
        // Sparse frames list `[register, slot]` pairs in register order.
        let slot = slots
            .iter()
            .find(|pair| pair[0].as_u64() == Some(register as u64))
            .map(|pair| &pair[1])
            .expect("a live float register has a recipe");
        assert_eq!(slot["representation"], "float64");
        assert_eq!(slot["locationKind"], "stackSlot");
        let offset = slot["locationValue"]
            .as_str()
            .expect("stack offset")
            .parse::<u32>()
            .expect("nonnegative stack offset");
        if let Some(previous) = homes_by_value.insert(value, offset) {
            assert_eq!(previous, offset, "one SSA value has one canonical home");
        }
    }
    let float_homes: BTreeSet<u32> = homes_by_value.values().copied().collect();
    assert_eq!(
        float_homes.len(),
        homes_by_value.len(),
        "overlapping values cannot share homes"
    );
    assert!(float_homes.len() >= FLOATS, "{state}");
    // Records root exact subsets of the tagged homes plus the exception
    // scratch; every untagged home lies beyond the highest rooted slot.
    let safepoints = json(bundle, JitArtifactFileName::Safepoints);
    let tagged_end = safepoints["records"]
        .as_array()
        .expect("safepoints")
        .iter()
        .filter_map(|point| point["taggedLocations"].as_array())
        .flatten()
        .map(|location| {
            assert_eq!(location["kind"], "spillSlot");
            location["index"].as_u64().expect("spill index") + 1
        })
        .max()
        .expect("a rooted exception scratch");
    assert!(
        float_homes
            .iter()
            .all(|&offset| u64::from(offset) >= tagged_end * 8 && offset % 8 == 0),
        "untagged homes must lie beyond every collector-rooted home",
    );

    let code = bundle
        .file(JitArtifactFileName::Code)
        .expect("native code")
        .contents();
    let start = operation["startOffset"].as_u64().expect("node start") as usize;
    let end = operation["endOffset"].as_u64().expect("node end") as usize;
    #[cfg(target_arch = "aarch64")]
    {
        // AArch64 unsigned-offset STR/LDR D. These are the canonical-home
        // stores and reloads in this operation; its fast path uses only GP words.
        let words: Vec<u32> = code[start..end]
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect();
        assert!(words.iter().any(|word| word & 0xffc0_0000 == 0xfd00_0000));
        assert!(words.iter().any(|word| word & 0xffc0_0000 == 0xfd40_0000));
    }
    #[cfg(target_arch = "x86_64")]
    {
        let instructions = native_code::decode(code, start, end);
        for store in [true, false] {
            let touched: BTreeSet<u32> = instructions
                .iter()
                .filter_map(|instruction| {
                    if instruction.opcode() != Opcode::MOVSD {
                        return None;
                    }
                    let memory = instruction.operand(if store { 0 } else { 1 });
                    let register = instruction.operand(if store { 1 } else { 0 });
                    if !matches!(register, Operand::Register { reg }
                    if reg.class() == RegSpec::xmm(0).class())
                    {
                        return None;
                    }
                    native_code::base_offset(memory)
                        .filter(|(base, offset)| *base == RegSpec::rsp() && *offset >= 0)
                        .map(|(_, offset)| offset as u32)
                })
                .collect();
            // Register-resident values owe stores/reloads at this collecting
            // operation. Already-spilled values keep their distinct homes.
            assert!(
                !touched.is_empty(),
                "native floating canonical-home {store:?}"
            );
            assert!(
                touched
                    .iter()
                    .all(|offset| u64::from(*offset) >= tagged_end * 8 && offset % 8 == 0),
                "floating homes lie outside tagged roots"
            );
            assert!(
                touched.iter().any(|offset| float_homes.contains(offset)),
                "native floating access reaches an exact owed arithmetic home"
            );
        }
    }
    last_loop_pc
}

#[test]
fn canonical_float_homes_and_young_carrier_survive_nested_osr_moving_gc() {
    let collecting_calls = if stress_stride() == 0 { 24_576 } else { 512 };
    let setup = setup_source(collecting_calls);
    let expected = format!(
        "[[335446016.5,32768,32768],32768,{}]",
        collecting_calls * 16
    );
    let mut oracle = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .expect("interpreter oracle");
    oracle
        .run_script(SourceInput::from_javascript(setup.clone()), MODULE)
        .expect("oracle setup");
    let oracle = oracle
        .run_script(
            SourceInput::from_javascript(PROBE),
            "canonical-homes-probe.js",
        )
        .expect("oracle pressure probe")
        .completion_string()
        .to_owned();
    assert_eq!(oracle, expected);

    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .expect("canonical-home runtime");
    runtime
        .run_script(SourceInput::from_javascript(setup), MODULE)
        .expect("warm custom Instanceof callback");
    let before = runtime.execution_stats();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(PROBE),
            "canonical-homes-probe.js",
        )
        .expect("compiled pressure probe");
    let after = runtime.execution_stats();
    assert_eq!(result.completion_string(), oracle);
    assert!(after.jit_optimized_osr_entries > before.jit_optimized_osr_entries);
    assert!(after.gc_minor_cycles > before.gc_minor_cycles);
    assert!(after.gc_minor_slot_updates - before.gc_minor_slot_updates >= 2);
    assert!(after.jit_reentrant_stub_transitions > before.jit_reentrant_stub_transitions);

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
    assert!(
        !own.is_empty(),
        "the once-called function must own optimizing OSR code"
    );
    let function_id = own[0].manifest().function_id();
    let last_loop_pc = own
        .iter()
        .map(|bundle| assert_pressure_artifact(bundle))
        .max()
        .unwrap();
    let report = result.jit_debug_report().expect("probe events");
    assert!(
        !report.truncated(),
        "native-execution evidence must be complete"
    );
    for event in report.events() {
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
                "optimizing loop exited before its allocating phase: {event:?}"
            );
            assert_eq!(reason, ExitReason::InsufficientFeedback, "{event:?}");
        }
    }
    drop(result);
    runtime.force_gc().expect("completed native frames");
    let retained = runtime
        .run_script(
            SourceInput::from_javascript(
                "sink[15].keep.marker + ':' + (sink[15].keep.next === input);",
            ),
            "canonical-homes-retained.js",
        )
        .expect("retained carrier after full GC");
    assert_eq!(retained.completion_string(), "7:true");
}
