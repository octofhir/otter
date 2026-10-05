//! Native Graph literals across real OSR and moving collection.
//!
//! # Contents
//! - Once-called nested loops allocating empty and nonempty object/array cells.
//! - Default and additional source realms, exact interpreter parity.
//! - Typed Graph regions, direct cold span entries and canonical home proof.
//! - Boxed/numeric element bases reloaded after collecting literal/instanceof nodes.
//! - Optional raw probe artifacts preserved before assertions for gate diagnosis.
//!
//! # Invariants
//! Only the measured function loops during each probe, so actual optimizing
//! OSR belongs to that function. Allocating `Symbol.hasInstance` calls start
//! late in its loop, after warmup; relocation counters are sampled around the
//! probe. Empty literal nodes have no register-window inputs; nonempty nodes
//! consume initialized canonical tagged homes. Array allocation recipes owe
//! earlier live object and array definitions in their exact traced homes.
//! Pre-collection element reads retain child aliases and numeric bit patterns;
//! native stores and reads after collection must use the relocated slab base.
//!
//! # See also
//! `jit_literal_allocation` covers sparse/nonempty boxed-value spans;
//! `otter-jit/src/arm64/allocation/empty_tests.rs` executes fit/miss geometry.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

#[cfg(target_arch = "x86_64")]
#[path = "support/native_code.rs"]
mod native_code;

#[cfg(target_arch = "x86_64")]
use yaxpeax_arch::LengthedInstruction;
#[cfg(target_arch = "x86_64")]
use yaxpeax_x86::amd64::{Opcode, Operand, RegSpec};

use std::collections::BTreeMap;
use std::fmt::Write;

use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitDebugTarget,
    JitDebugTier, JitSelection, Runtime, RuntimeRealmId, SourceInput,
};
use otter_vm::native_abi::{
    ExitReason, STUB_ALLOC_GROUP_ENSURE, STUB_JIT_NEW_ARRAY, STUB_JIT_NEW_OBJECT,
    STUB_JIT_NEW_OBJECT_LITERAL,
};
use serde_json::Value as Json;

const FUNCTION: &str = "emptyAllocationsOnce";
const MODULE: &str = "native-empty-setup.js";
const ITERATIONS: usize = 8 * 4096;
const PROBE: &str = "JSON.stringify([emptyAllocationsOnce(input, target), calls, allocations]);";

fn setup(collecting_calls: usize) -> String {
    let mut source = format!(
        "const input = {{tag: 7}};\nconst sink = new Array(16);\n\
         let calls = 0, allocations = 0;\nconst allocationGate = {};\n\
         const target = {{ [Symbol.hasInstance](value) {{\n\
         calls++;\nif (calls > allocationGate) {{\n",
        ITERATIONS - collecting_calls,
    );
    // No helper loop can claim an OSR entry during the measured probe.
    for index in 0..16 {
        writeln!(source, "sink[{index}] = {{keep: value, padding: calls}};").unwrap();
    }
    // StoreElement records the receiver family before mutation. Grow the
    // fresh empty literal through push first so the following exact store
    // observes tagged storage during the early iterations before Graph OSR.
    source.push_str(
        "allocations += 16;\n}\nreturn value.marker === 7;\n} };\n\
         function emptyAllocationsOnce(input, target) {\n\
         let hits = 0, links = 0;\n\
         for (let outer = 0; outer < 8; outer++) {\n\
         for (let inner = 0; inner < 4096; inner++) {\n\
         try {\nconst object = {};\nconst array = [];\n\
         object.marker = 7; object.keep = input; object.array = array;\n\
         array.push(object);\narray[0] = object;\nconst hit = object instanceof target;\n\
         hits += hit ? 1 : 0;\n\
         links += array[0] === object && object.keep === input && object.array === array ? 1 : 0;\n\
         } catch (error) { throw error; }\n}\n}\nreturn [hits, links];\n}\n\
         const warmCarrier = {marker: 7};\n\
         for (let warm = 0; warm < 5000; warm++) warmCarrier instanceof target;\n\
         calls = 0;\n",
    );
    source
}

fn run(
    runtime: &mut Runtime,
    realm: Option<RuntimeRealmId>,
    source: &str,
    module: &str,
) -> otter_runtime::ExecutionResult {
    match realm {
        Some(realm) => {
            runtime.run_script_in_realm(realm, SourceInput::from_javascript(source), module)
        }
        None => runtime.run_script(SourceInput::from_javascript(source), module),
    }
    .expect("empty-literal script")
}

fn json(bundle: &JitArtifactBundle, name: JitArtifactFileName) -> Json {
    serde_json::from_slice(bundle.file(name).expect("artifact payload").contents())
        .expect("artifact JSON")
}

/// Preserve every captured generation before a proof can panic. Gate runners
/// provide a fresh output directory; ordinary test runs perform no file I/O.
fn preserve_probe(result: &otter_runtime::ExecutionResult, case: &str, extra_realm: bool) {
    let Some(root) = std::env::var_os("OTTER_LITERAL_PROOF_ARTIFACTS") else {
        return;
    };
    let realm = if extra_realm { "additional" } else { "default" };
    let directory = std::path::PathBuf::from(root).join(format!("{case}-{realm}"));
    std::fs::create_dir_all(directory.parent().unwrap()).unwrap();
    std::fs::create_dir(&directory).expect("fresh probe evidence directory");
    std::fs::write(directory.join("completion.txt"), result.completion_string()).unwrap();
    if let Some(report) = result.jit_debug_report() {
        std::fs::write(
            directory.join("events.json"),
            serde_json::to_vec_pretty(report).unwrap(),
        )
        .unwrap();
    }
    let batch = result.jit_artifacts().expect("captured probe artifacts");
    std::fs::write(
        directory.join("capture.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "bundles": batch.bundles().len(),
            "retainedBytes": batch.retained_bytes(),
            "droppedBundles": batch.dropped_bundles(),
            "droppedBytes": batch.dropped_bytes(),
            "truncated": batch.truncated(),
        }))
        .unwrap(),
    )
    .unwrap();
    for bundle in batch.bundles() {
        let compile = directory.join(format!("code-{}", bundle.manifest().code_object_id()));
        std::fs::create_dir(&compile).unwrap();
        std::fs::write(
            compile.join("manifest.json"),
            serde_json::to_vec_pretty(bundle.manifest()).unwrap(),
        )
        .unwrap();
        for file in bundle.files() {
            std::fs::write(compile.join(file.name().as_str()), file.contents()).unwrap();
        }
    }
}

fn assert_native_empty_artifact(bundle: &JitArtifactBundle, extra_realm: bool) -> u32 {
    let ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let formed = ir.lines().any(|line| line.contains(" = AllocationGroup("));
    if formed {
        assert!(
            !extra_realm,
            "additional-realm array remains a canonical sidecar allocation"
        );
        return assert_native_empty_group_artifact(bundle);
    }
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().expect("native regions");
    let own = |kind: &str| {
        regions
            .iter()
            .find(|region| {
                region["kind"] == "instruction"
                    && region["operation"]
                        .as_str()
                        .is_some_and(|name| name.ends_with(kind))
            })
            .unwrap_or_else(|| panic!("own Graph {kind} node"))
    };
    let object = own(" NewObject");
    let array = own(" NewArrayEmpty");
    let object_id = object["operationIndex"].as_u64().unwrap() as u32;
    let array_id = array["operationIndex"].as_u64().unwrap() as u32;
    let array_pc = array["logicalPc"].as_u64().unwrap() as u32;
    let ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("Graph IR")
            .contents(),
    )
    .unwrap();
    let nodes: BTreeMap<u32, &str> = ir
        .lines()
        .filter_map(|line| {
            let (id, body) = line.trim().strip_prefix('v')?.split_once(" = ")?;
            Some((id.parse().unwrap(), body))
        })
        .collect();
    for (id, kind) in [(object_id, "NewObject"), (array_id, "NewArrayEmpty")] {
        assert!(
            nodes[&id].starts_with(&format!("{kind} []")),
            "native allocation takes no interpreter window: {}",
            nodes[&id]
        );
    }

    let relocations = json(bundle, JitArtifactFileName::Relocations);
    for (region, stub) in [(object, STUB_JIT_NEW_OBJECT), (array, STUB_JIT_NEW_ARRAY)] {
        let start = region["startOffset"].as_u64().unwrap();
        let end = region["endOffset"].as_u64().unwrap();
        assert!(
            relocations["relocations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| {
                    row["target"]["id"].as_u64() == Some(u64::from(stub.id))
                        && row["target"]["signature"] == "reentrantValueSpan"
                        && row["startOffset"]
                            .as_u64()
                            .is_some_and(|offset| start <= offset && offset < end)
                }),
            "native allocation keeps its direct committed cold entry {stub:?}"
        );
    }

    let eager = nodes[&array_id]
        .split_once(" eager=")
        .expect("array eager state")
        .1;
    let registers: Vec<usize> = eager
        .split('(')
        .skip(1)
        .filter_map(|tuple| {
            let (register, value) = tuple.split_once(')').unwrap().0.split_once(',').unwrap();
            (value.trim().parse::<u32>().unwrap() == object_id)
                .then(|| register.trim().parse().unwrap())
        })
        .collect();
    assert!(
        !registers.is_empty(),
        "array allocation must owe the live fresh object"
    );
    let deopt = json(bundle, JitArtifactFileName::Deopt);
    let exit = deopt["exits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|exit| {
            exit["reason"] == "runtimeTransition"
                && exit["resumePcs"].as_array().and_then(|pcs| pcs.last())
                    == Some(&Json::from(array_pc))
        })
        .expect("array's exact throwing allocation recipe");
    let state_id = exit["frameStateId"].as_u64().unwrap();
    let state = deopt["frameStates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["id"].as_u64() == Some(state_id))
        .unwrap();
    let frames = state["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1, "probe owns the empty allocations");
    assert_eq!(
        frames[0]["functionId"].as_u64(),
        Some(u64::from(bundle.manifest().function_id()))
    );
    let slots = frames[0]["slots"].as_array().unwrap();
    let home = slots[registers[0]]["locationValue"]
        .as_str()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    for register in registers {
        assert_eq!(slots[register]["representation"], "tagged");
        assert_eq!(slots[register]["locationKind"], "stackSlot");
        assert_eq!(
            slots[register]["locationValue"]
                .as_str()
                .unwrap()
                .parse::<u32>()
                .unwrap(),
            home
        );
    }
    let points = json(bundle, JitArtifactFileName::Safepoints);
    let roots = points["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|point| point["id"].as_u64() == Some(u64::from(u32::MAX - 1)))
        .expect("canonical initialized body roots");
    let tagged = roots["taggedLocations"].as_array().unwrap();
    assert!(
        tagged
            .iter()
            .enumerate()
            .all(|(index, root)| root["kind"] == "spillSlot"
                && root["index"].as_u64() == Some(index as u64))
    );
    assert!(home.is_multiple_of(8) && (home as usize / 8) < tagged.len());
    let code = bundle
        .file(JitArtifactFileName::Code)
        .expect("native code")
        .contents();
    assert_empty_native_bytes(code, [object, array], array, home, extra_realm);
    regions
        .iter()
        .filter(|region| {
            region["operation"]
                .as_str()
                .is_some_and(|name| name.contains(" JumpLoop("))
        })
        .map(|region| region["logicalPc"].as_u64().unwrap() as u32)
        .max()
        .expect("own nested backedges")
}

/// A folded pair preserves the original source ids/PCs and complete cell
/// recipes. Its collecting boundary precedes both cells; the first later
/// collecting instanceof must root the now-published object aliases.
fn assert_native_empty_group_artifact(bundle: &JitArtifactBundle) -> u32 {
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().unwrap();
    let ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let nodes: BTreeMap<u32, &str> = ir
        .lines()
        .filter_map(|line| {
            let (id, body) = line.trim().strip_prefix('v')?.split_once(" = ")?;
            Some((id.parse().unwrap(), body))
        })
        .collect();
    let own = |kind: &str| {
        regions
            .iter()
            .filter(|r| {
                r["kind"] == "instruction"
                    && r["functionId"] == bundle.manifest().function_id()
                    && r["operation"]
                        .as_str()
                        .is_some_and(|text| text.contains(kind))
            })
            .collect::<Vec<_>>()
    };
    let groups = own(" AllocationGroup(");
    assert_eq!(groups.len(), 1, "own object+array group: {ir}");
    let group = groups[0];
    let object_id = group["operationIndex"].as_u64().unwrap() as u32;
    let index = nodes[&object_id]
        .strip_prefix("AllocationGroup(")
        .unwrap()
        .split_once(')')
        .unwrap()
        .0;
    assert!(
        nodes[&object_id].contains(") [] Tagged eager="),
        "single original no-window eager group: {ir}"
    );
    let projections = own(" AllocationProjection(");
    assert_eq!(projections.len(), 1, "exact second source cell: {ir}");
    let array = projections[0];
    let array_id = array["operationIndex"].as_u64().unwrap() as u32;
    let object_bytes = otter_vm::jit::JitEmptyObjectAllocationPlan::new(8).cell_bytes;
    let array_bytes = otter_vm::jit::JitEmptyArrayAllocationPlan::default().cell_bytes;
    assert_eq!(
        nodes[&array_id],
        format!("AllocationProjection({object_bytes}) [{object_id}] Tagged")
    );
    assert!(group["logicalPc"].as_u64().unwrap() < array["logicalPc"].as_u64().unwrap());
    assert!(group["bytePc"].as_u64().unwrap() < array["bytePc"].as_u64().unwrap());
    let header = format!(
        "; allocation-group {index} realm=0 bytes={}",
        object_bytes + array_bytes
    );
    let mut members = ir.lines().skip_while(|line| *line != header).skip(1);
    assert!(
        members
            .next()
            .unwrap()
            .starts_with(&format!("; member v{object_id} offset=0 layout=Object("))
    );
    assert!(members.next().unwrap().starts_with(&format!(
        "; member v{array_id} offset={object_bytes} layout=Array("
    )));
    assert!(
        !members.next().unwrap().starts_with("; member "),
        "only the two source cells belong to this group"
    );
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let start = group["startOffset"].as_u64().unwrap();
    let end = group["endOffset"].as_u64().unwrap();
    assert!(
        relocations["relocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["target"]["id"] == STUB_ALLOC_GROUP_ENSURE.id
                && r["target"]["signature"] == "allocValue3"
                && r["startOffset"]
                    .as_u64()
                    .is_some_and(|offset| start <= offset && offset < end)),
        "own folded body uses the sole noncharging admission boundary"
    );
    let collecting = own(" Instanceof");
    assert_eq!(collecting.len(), 1);
    let collecting = collecting[0];
    let collecting_id = collecting["operationIndex"].as_u64().unwrap() as u32;
    let eager = nodes[&collecting_id].split_once(" eager=").unwrap().1;
    let registers: Vec<usize> = eager
        .split('(')
        .skip(1)
        .filter_map(|tuple| {
            let (r, v) = tuple.split_once(')').unwrap().0.split_once(',').unwrap();
            (v.trim().parse::<u32>().unwrap() == object_id).then(|| r.trim().parse().unwrap())
        })
        .collect();
    assert!(
        !registers.is_empty(),
        "the later collecting operation owes the actual first tagged projection"
    );
    let deopt = json(bundle, JitArtifactFileName::Deopt);
    assert!(
        deopt["exits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["reason"] == "allocationMiss"
                && e["action"] == "resume"
                && e["resumePcs"].as_array().unwrap().last() == Some(&group["logicalPc"])),
        "every group admission refusal resumes the first original allocation before effects"
    );
    let exit = deopt["exits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["reason"] == "runtimeTransition"
                && e["resumePcs"].as_array().unwrap().last() == Some(&collecting["logicalPc"])
        })
        .unwrap();
    let state = deopt["frameStates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == exit["frameStateId"])
        .unwrap();
    let frames = state["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["functionId"], bundle.manifest().function_id());
    let slots = frames[0]["slots"].as_array().unwrap();
    let home = slots[registers[0]]["locationValue"]
        .as_str()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    for register in registers {
        assert_eq!(slots[register]["representation"], "tagged");
        assert_eq!(slots[register]["locationKind"], "stackSlot");
        assert_eq!(
            slots[register]["locationValue"]
                .as_str()
                .unwrap()
                .parse::<u32>()
                .unwrap(),
            home
        );
    }
    let points = json(bundle, JitArtifactFileName::Safepoints);
    let body = points["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == u32::MAX - 1)
        .unwrap();
    let tagged = body["taggedLocations"].as_array().unwrap();
    assert!(
        tagged
            .iter()
            .enumerate()
            .all(|(i, r)| r["kind"] == "spillSlot" && r["index"] == i as u64)
    );
    assert!(home.is_multiple_of(8) && (home as usize / 8) < tagged.len());
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let [object_init, array_init] = split_group_cell_initializers(code, group, object_bytes);
    assert_empty_native_bytes(code, [&object_init, &array_init], collecting, home, false);
    regions
        .iter()
        .filter(|r| {
            r["operation"]
                .as_str()
                .is_some_and(|name| name.contains(" JumpLoop("))
        })
        .map(|r| r["logicalPc"].as_u64().unwrap() as u32)
        .max()
        .expect("own nested backedges")
}

#[cfg(target_arch = "aarch64")]
fn split_group_cell_initializers(code: &[u8], region: &Json, object_bytes: u32) -> [Json; 2] {
    let start = region["startOffset"].as_u64().unwrap() as usize;
    let end = region["endOffset"].as_u64().unwrap() as usize;
    let words: Vec<_> = code[start..end]
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let headers: Vec<_> = words
        .iter()
        .enumerate()
        .filter_map(|(i, &word)| {
            let base = (word >> 5) & 31;
            (word & 0xffff_fc1f == 0xf900_0010 && ![16, 20, 21, 31].contains(&base))
                .then_some((i, base))
        })
        .collect();
    assert_eq!(
        headers.len(),
        4,
        "two complete initializers in the first fit and the post-ensure fit"
    );
    assert_eq!(headers[0].1, headers[1].1);
    let (first, candidate) = headers[0];
    let second = headers[1].0;
    assert!(
        words[first + 1..second]
            .iter()
            .any(|&word| word == 0x9100_0000 | (object_bytes << 10) | (candidate << 5) | candidate),
        "the array candidate advances by the exact first physical cell before its header"
    );
    let mut first_region = region.clone();
    first_region["startOffset"] = Json::from(start + first * 4);
    first_region["endOffset"] = Json::from(start + second * 4);
    let mut second_region = region.clone();
    second_region["startOffset"] = Json::from(start + second * 4);
    second_region["endOffset"] = Json::from(start + headers[2].0 * 4);
    [first_region, second_region]
}
#[cfg(target_arch = "x86_64")]
fn split_group_cell_initializers(code: &[u8], region: &Json, object_bytes: u32) -> [Json; 2] {
    let start = region["startOffset"].as_u64().unwrap() as usize;
    let end = region["endOffset"].as_u64().unwrap() as usize;
    let instructions = native_code::decode(code, start, end);
    let headers: Vec<_> = instructions
        .iter()
        .enumerate()
        .filter_map(|(i, instruction)| {
            (instruction.opcode() == Opcode::MOV
                && instruction.operand(1)
                    == Operand::Register {
                        reg: RegSpec::r10(),
                    })
            .then(|| native_code::base_offset(instruction.operand(0)))
            .flatten()
            .filter(|(base, offset)| {
                *offset == 0
                    && base.class() == RegSpec::rax().class()
                    && ![4, 5, 10, 11, 13, 14, 15].contains(&base.num())
            })
            .map(|(base, _)| (i, base))
        })
        .collect();
    assert_eq!(
        headers.len(),
        4,
        "two complete initializers in the first fit and the post-ensure fit"
    );
    assert_eq!(headers[0].1, headers[1].1);
    let (first, candidate) = headers[0];
    let second = headers[1].0;
    assert!(
        instructions[first + 1..second]
            .iter()
            .any(|i| i.opcode() == Opcode::ADD
                && i.operand(0) == Operand::Register { reg: candidate }
                && match i.operand(1) {
                    Operand::ImmediateI8 { imm } => i32::from(imm) == object_bytes as i32,
                    Operand::ImmediateI32 { imm } => imm == object_bytes as i32,
                    Operand::ImmediateI64 { imm } => imm == i64::from(object_bytes),
                    _ => false,
                }),
        "the array candidate advances by the exact first physical cell before its header"
    );
    let mut first_region = region.clone();
    first_region["startOffset"] = Json::from(
        start
            + instructions[..first]
                .iter()
                .map(|i| i.len().to_const() as usize)
                .sum::<usize>(),
    );
    let second_start = start
        + instructions[..second]
            .iter()
            .map(|i| i.len().to_const() as usize)
            .sum::<usize>();
    first_region["endOffset"] = Json::from(second_start);
    let mut second_region = region.clone();
    second_region["startOffset"] = Json::from(second_start);
    second_region["endOffset"] = Json::from(
        start
            + instructions[..headers[2].0]
                .iter()
                .map(|i| i.len().to_const() as usize)
                .sum::<usize>(),
    );
    [first_region, second_region]
}

#[cfg(target_arch = "aarch64")]
fn assert_empty_native_bytes(
    code: &[u8],
    regions: [&Json; 2],
    home_region: &Json,
    home: u32,
    extra_realm: bool,
) {
    let word_regions = regions.map(|region| {
        let start = region["startOffset"].as_u64().unwrap() as usize;
        let end = region["endOffset"].as_u64().unwrap() as usize;
        code[start..end]
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect::<Vec<_>>()
    });
    assert!(
        code[home_region["startOffset"].as_u64().unwrap() as usize
            ..home_region["endOffset"].as_u64().unwrap() as usize]
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .any(|word| word & 0xffc0_03e0 == 0xf900_03e0
                && ((word >> 10) & 4095) * 8 == home
                && word & 31 != 31),
        "definition/collecting save stores the exact live object home"
    );
    // The object fit always initializes a cell. A default-realm array does
    // too; additional-realm arrays intentionally take their sidecar allocator.
    for (index, words) in word_regions
        .iter()
        .take(if extra_realm { 1 } else { 2 })
        .enumerate()
    {
        let candidate = words
            .iter()
            .find_map(|&word| {
                let base = (word >> 5) & 31;
                (word & 0xffff_fc1f == 0xf900_0010 && ![16, 20, 21, 31].contains(&base))
                    .then_some(base)
            })
            .expect("exact header STR x16 into an allocator-owned candidate");
        let offsets: Vec<u32> = if index == 0 {
            otter_vm::jit::JitEmptyObjectAllocationPlan::new(8)
                .initial_value_bytes
                .to_vec()
        } else {
            (8..otter_vm::jit::JitEmptyArrayAllocationPlan::default().cell_bytes)
                .step_by(8)
                .collect()
        };
        assert!(
            offsets.iter().all(|&byte| words.iter().any(|&word| word
                == 0xf900_0000
                    | ((byte / 8) << 10)
                    | (candidate << 5)
                    | if index == 0 { 16 } else { 31 })),
            "every VM-owned initial Value/payload word is emitted into the fit candidate"
        );
    }
}

#[cfg(target_arch = "x86_64")]
fn assert_empty_native_bytes(
    code: &[u8],
    regions: [&Json; 2],
    home_region: &Json,
    home: u32,
    extra_realm: bool,
) {
    let instruction_regions = regions.map(|region| {
        native_code::decode(
            code,
            region["startOffset"].as_u64().unwrap() as usize,
            region["endOffset"].as_u64().unwrap() as usize,
        )
    });
    assert!(
        native_code::decode(
            code,
            home_region["startOffset"].as_u64().unwrap() as usize,
            home_region["endOffset"].as_u64().unwrap() as usize
        )
        .iter()
        .any(|instruction| instruction.opcode() == Opcode::MOV
            && native_code::base_offset(instruction.operand(0))
                == Some((RegSpec::rsp(), home as i32))
            && matches!(instruction.operand(1), Operand::Register { reg }
                if reg.class() == RegSpec::rax().class())),
        "definition/collecting save stores the exact live object home"
    );
    for (index, instructions) in instruction_regions
        .iter()
        .take(if extra_realm { 1 } else { 2 })
        .enumerate()
    {
        let candidate = instructions
            .iter()
            .find_map(|instruction| {
                if instruction.opcode() != Opcode::MOV
                    || instruction.operand(1)
                        != (Operand::Register {
                            reg: RegSpec::r10(),
                        })
                {
                    return None;
                }
                native_code::base_offset(instruction.operand(0))
                    .filter(|(base, displacement)| {
                        *displacement == 0
                            && base.class() == RegSpec::rax().class()
                            && ![4, 5, 10, 11, 13, 14, 15].contains(&base.num())
                    })
                    .map(|(base, _)| base)
            })
            .expect("exact header MOV r10 into allocator-owned candidate");
        let offsets: Vec<u32> = if index == 0 {
            otter_vm::jit::JitEmptyObjectAllocationPlan::new(8)
                .initial_value_bytes
                .to_vec()
        } else {
            (8..otter_vm::jit::JitEmptyArrayAllocationPlan::default().cell_bytes)
                .step_by(8)
                .collect()
        };
        for byte in offsets {
            assert!(
                instructions
                    .iter()
                    .any(|instruction| instruction.opcode() == Opcode::MOV
                        && instruction.mem_size().and_then(|size| size.bytes_size()) == Some(8)
                        && native_code::base_offset(instruction.operand(0))
                            == Some((candidate, byte as i32))
                        && if index == 0 {
                            instruction.operand(1)
                                == (Operand::Register {
                                    reg: RegSpec::r10(),
                                })
                        } else {
                            matches!(
                                instruction.operand(1),
                                Operand::ImmediateI8 { imm: 0 }
                                    | Operand::ImmediateI32 { imm: 0 }
                                    // REX.W MOV r/m64, imm32 sign-extends the
                                    // source; the decoder exposes that i64.
                                    | Operand::ImmediateI64 { imm: 0 }
                            )
                        }),
                "VM-owned initial payload word at byte {byte} must be emitted into {candidate:?} for literal {index}: {instructions:?}"
            );
        }
    }
}

fn probe(extra_realm: bool) {
    let stress = std::env::var("OTTER_GC_STRESS").is_ok_and(|value| value.trim() != "0");
    let collecting_calls = if stress { 512 } else { 24_576 };
    let source = setup(collecting_calls);
    let expected = format!("[[32768,32768],32768,{}]", collecting_calls * 16);
    let mut oracle = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .unwrap();
    let oracle_realm = extra_realm.then(|| oracle.create_realm().unwrap());
    run(&mut oracle, oracle_realm, &source, MODULE);
    let expected_actual = run(&mut oracle, oracle_realm, PROBE, "native-empty-probe.js")
        .completion_string()
        .to_owned();
    assert_eq!(expected_actual, expected);

    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let realm = extra_realm.then(|| runtime.create_realm().unwrap());
    run(&mut runtime, realm, &source, MODULE);
    let before = runtime.execution_stats();
    let result = run(&mut runtime, realm, PROBE, "native-empty-probe.js");
    let after = runtime.execution_stats();
    preserve_probe(&result, "empty", extra_realm);
    assert_eq!(result.completion_string(), expected_actual);
    assert!(after.jit_optimized_osr_entries > before.jit_optimized_osr_entries);
    assert!(after.gc_minor_cycles > before.gc_minor_cycles);
    assert!(after.gc_minor_slot_updates - before.gc_minor_slot_updates >= 2);
    assert!(after.jit_reentrant_stub_transitions > before.jit_reentrant_stub_transitions);
    let batch = result
        .jit_artifacts()
        .expect("compiled empty allocation artifacts");
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
        "the once-called subject must own actual Graph OSR"
    );
    let function_id = own[0].manifest().function_id();
    let last_loop_pc = own
        .iter()
        .map(|bundle| assert_native_empty_artifact(bundle, extra_realm))
        .max()
        .unwrap();
    let report = result.jit_debug_report().expect("native execution events");
    assert!(!report.truncated());
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
                "own Graph loop left before collecting phase: {event:?}"
            );
            assert_eq!(reason, ExitReason::InsufficientFeedback, "{event:?}");
        }
    }
    drop(result);
    runtime
        .force_gc()
        .expect("full collection after native return");
    let retained = run(
        &mut runtime,
        realm,
        "const kept = sink[15].keep; JSON.stringify([kept.marker, kept.keep === input, kept.array[0] === kept, Object.getPrototypeOf(kept) === Object.prototype, Object.getPrototypeOf(kept.array) === Array.prototype]);",
        "native-empty-retained.js",
    );
    assert_eq!(retained.completion_string(), "[7,true,true,true,true]");
}

#[test]
fn graph_empty_literals_survive_real_osr_and_moving_gc() {
    probe(false);
}

#[test]
fn graph_empty_literals_keep_their_additional_source_realm() {
    probe(true);
}

const NONEMPTY_FUNCTION: &str = "nonemptyAllocationsOnce";
const NONEMPTY_MODULE: &str = "native-nonempty-setup.js";
const NONEMPTY_PROBE: &str = "JSON.stringify([nonemptyAllocationsOnce(input, target, 0 / 0, -0, 1 / 0), calls, allocations]);";

fn nonempty_setup(collecting_calls: usize) -> String {
    let mut source = format!(
        "const input = {{tag: 7}};\nconst sink = new Array(16);\n\
         let calls = 0, allocations = 0;\nconst allocationGate = {};\n\
         const target = {{ [Symbol.hasInstance](value) {{\n\
         calls++;\nif (calls > allocationGate) {{\n",
        ITERATIONS - collecting_calls,
    );
    for index in 0..16 {
        writeln!(source, "sink[{index}] = {{keep: value, padding: calls}};").unwrap();
    }
    source.push_str(
        "allocations += 16;\n}\nreturn value.marker === 7;\n} };\n\
         function nonemptyAllocationsOnce(input, target, nan, negativeZero, infinity) {\n\
         let hits = 0, links = 0, holes = 0, numbers = 0;\n\
         for (let outer = 0; outer < 8; outer++) {\n\
         for (let inner = 0; inner < 4096; inner++) {\n\
         try {\nconst object = {marker: 7, keep: input};\n\
         const tagged = [object, , input, object];\n\
         const taggedBefore = tagged[0];\n\
         const packed = [nan, negativeZero, infinity, -infinity, 1.5];\n\
         const packedNanBefore = packed[0], packedZeroBefore = packed[1];\n\
         const holey = [nan, , negativeZero, infinity];\n\
         object.tagged = tagged; object.packed = packed; object.holey = holey;\n\
         const boxedBefore = tagged[3];\n\
         const numericNanBefore = packed[0], numericZeroBefore = packed[1];\n\
         const hit = object instanceof target;\n\
         tagged[3] = taggedBefore; packed[0] = numericNanBefore; packed[1] = numericZeroBefore;\n\
         hits += hit ? 1 : 0;\n\
         links += taggedBefore === object && boxedBefore === object && tagged[0] === object && tagged[2] === input && tagged[3] === object && object.keep === input ? 1 : 0;\n\
         holes += !(1 in tagged) && !(1 in holey) && 0 in packed && tagged.length === 4 && holey.length === 4 ? 1 : 0;\n\
         numbers += packedNanBefore !== packedNanBefore && numericNanBefore !== numericNanBefore && 1 / packedZeroBefore === -infinity && 1 / numericZeroBefore === -infinity && packed[0] !== packed[0] && 1 / packed[1] === -infinity && packed[2] === infinity && packed[3] === -infinity && packed[4] === 1.5 && holey[0] !== holey[0] && 1 / holey[2] === -infinity ? 1 : 0;\n\
         } catch (error) { throw error; }\n}\n}\nreturn [hits, links, holes, numbers];\n}\n\
         const warmCarrier = {marker: 7};\n\
         for (let warm = 0; warm < 5000; warm++) warmCarrier instanceof target;\n\
         calls = 0;\n",
    );
    source
}

fn ir_inputs(body: &str) -> Vec<u32> {
    let inputs = body.split_once('[').unwrap().1.split_once(']').unwrap().0;
    if inputs.trim().is_empty() {
        return Vec::new();
    }
    inputs
        .split(',')
        .map(|value| value.trim().trim_start_matches('v').parse().unwrap())
        .collect()
}

fn assert_element_bases_refresh_around_instanceof(regions: &[Json], nodes: &BTreeMap<u32, &str>) {
    let region_of = |id: u32| {
        regions
            .iter()
            .find(|region| {
                region["kind"] == "instruction"
                    && region["operationIndex"].as_u64() == Some(u64::from(id))
            })
            .expect("own emitted Graph node")
    };
    let boundary = nodes
        .iter()
        .find(|(_, body)| body.starts_with("Instanceof ["))
        .map(|(&id, _)| region_of(id))
        .expect("own collecting instanceof node");
    let before_boundary = boundary["startOffset"].as_u64().unwrap();
    let after_boundary = boundary["endOffset"].as_u64().unwrap();
    for representation in ["Boxed", "Float64"] {
        let owner = nodes
            .iter()
            .filter(|(_, body)| body.starts_with("NewArrayLiteral ["))
            .find(|(_, body)| {
                let inputs = ir_inputs(body);
                if representation == "Boxed" {
                    inputs.len() == 4 && nodes[&inputs[0]].starts_with("NewObjectLiteral [")
                } else {
                    inputs.len() == 5
                }
            })
            .map(|(&id, _)| id)
            .expect("own boxed/packed literal array");
        let bases: Vec<_> = nodes
            .iter()
            .filter(|(_, body)| body.starts_with("LoadElementsBase("))
            .filter(|(_, body)| ir_inputs(body) == [owner])
            .map(|(&id, _)| id)
            .collect();
        let base_before: Vec<_> = bases
            .iter()
            .copied()
            .filter(|&id| region_of(id)["startOffset"].as_u64().unwrap() < before_boundary)
            .collect();
        let base_after: Vec<_> = bases
            .iter()
            .copied()
            .filter(|&id| region_of(id)["startOffset"].as_u64().unwrap() >= after_boundary)
            .collect();
        assert!(
            !base_before.is_empty() && !base_after.is_empty(),
            "{representation} owner v{owner} reloads its slab base around collection"
        );
        assert!(base_before.iter().all(|id| !base_after.contains(id)));
        let load = format!("LoadElement({representation}) [");
        for (bases, before) in [(&base_before, true), (&base_after, false)] {
            assert!(
                nodes.iter().any(|(&id, body)| {
                    body.starts_with(&load)
                        && bases.contains(&ir_inputs(body)[0])
                        && if before {
                            region_of(id)["endOffset"].as_u64().unwrap() <= before_boundary
                        } else {
                            region_of(id)["startOffset"].as_u64().unwrap() >= after_boundary
                        }
                }),
                "native {representation} element read on each side of collection"
            );
        }
        let store = format!("StoreElement({representation}) [");
        assert!(
            nodes.iter().any(|(&id, body)| {
                body.starts_with(&store)
                    && base_after.contains(&ir_inputs(body)[0])
                    && region_of(id)["startOffset"].as_u64().unwrap() >= after_boundary
            }),
            "native {representation} store uses the post-collection slab base"
        );
    }
}

fn assert_native_nonempty_artifact(bundle: &JitArtifactBundle, extra_realm: bool) -> u32 {
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().expect("native regions");
    let last_loop_pc = regions
        .iter()
        .filter(|region| {
            region["operation"]
                .as_str()
                .is_some_and(|operation| operation.contains(" JumpLoop("))
        })
        .map(|region| region["logicalPc"].as_u64().unwrap() as u32)
        .max()
        .expect("own nested backedges");
    let literals: Vec<_> = regions
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["logicalPc"].as_u64().unwrap() <= u64::from(last_loop_pc)
                && region["operation"].as_str().is_some_and(|operation| {
                    operation.ends_with(" NewObjectLiteral")
                        || operation.ends_with(" NewArrayLiteral")
                })
        })
        .collect();
    assert_eq!(
        literals.len(),
        4,
        "one shaped object and three dense arrays"
    );
    let ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let nodes: BTreeMap<u32, &str> = ir
        .lines()
        .filter_map(|line| {
            let (id, body) = line.trim().strip_prefix('v')?.split_once(" = ")?;
            Some((id.parse().unwrap(), body))
        })
        .collect();
    assert_element_bases_refresh_around_instanceof(regions, &nodes);
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let relocations = relocations["relocations"].as_array().unwrap();
    let deopt = json(bundle, JitArtifactFileName::Deopt);
    let points = json(bundle, JitArtifactFileName::Safepoints);
    let roots = points["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|point| point["id"].as_u64() == Some(u64::from(u32::MAX - 1)))
        .expect("canonical initialized body roots");
    let roots = roots["taggedLocations"].as_array().unwrap();
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let mut counts = Vec::new();
    for region in literals {
        let id = region["operationIndex"].as_u64().unwrap() as u32;
        let body = nodes[&id];
        let object = body.starts_with("NewObjectLiteral [");
        assert!(object || body.starts_with("NewArrayLiteral ["));
        let inputs = ir_inputs(body);
        counts.push(inputs.len());
        let pc = region["logicalPc"].as_u64().unwrap();
        let start = region["startOffset"].as_u64().unwrap();
        let end = region["endOffset"].as_u64().unwrap();
        let stub = if object {
            STUB_JIT_NEW_OBJECT_LITERAL
        } else {
            STUB_JIT_NEW_ARRAY
        };
        assert!(
            relocations.iter().any(|row| {
                row["target"]["id"].as_u64() == Some(u64::from(stub.id))
                    && row["target"]["signature"] == "reentrantValueSpan"
                    && row["startOffset"]
                        .as_u64()
                        .is_some_and(|offset| start <= offset && offset < end)
            }),
            "own literal has its committed boxed cold span: {body}"
        );
        let exit = deopt["exits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|exit| {
                exit["reason"] == "runtimeTransition"
                    && exit["resumePcs"].as_array().and_then(|pcs| pcs.last())
                        == Some(&Json::from(pc))
            })
            .expect("literal's exact throwing state");
        let state = deopt["frameStates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|state| state["id"] == exit["frameStateId"])
            .unwrap();
        let frames = state["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 1, "subject owns native literal source");
        assert_eq!(
            frames[0]["functionId"].as_u64(),
            Some(u64::from(bundle.manifest().function_id()))
        );
        let slots = frames[0]["slots"].as_array().unwrap();
        let eager = body.split_once(" eager=").unwrap().1;
        let mut dynamic_inputs = 0;
        for input in inputs {
            if nodes[&input].starts_with("ConstTagged(") {
                continue;
            }
            dynamic_inputs += 1;
            let registers: Vec<usize> = eager
                .split('(')
                .skip(1)
                .filter_map(|tuple| {
                    let (register, value) =
                        tuple.split_once(')').unwrap().0.split_once(',').unwrap();
                    (value.trim().parse::<u32>().unwrap() == input)
                        .then(|| register.trim().parse().unwrap())
                })
                .collect();
            if registers.is_empty() {
                // A representation conversion is a native operand value,
                // while the eager recipe still owes its source VM register.
                assert!(
                    nodes[&input].starts_with("Int32ToTagged [")
                        || nodes[&input].starts_with("Float64ToTagged ["),
                    "nonconstant operand without its eager register is a tagged conversion: {}",
                    nodes[&input]
                );
            }
            for register in registers {
                let slot = &slots[register];
                assert_eq!(slot["representation"], "tagged", "bulk input v{input}");
                assert_eq!(slot["locationKind"], "stackSlot", "bulk input v{input}");
                let home: u32 = slot["locationValue"].as_str().unwrap().parse().unwrap();
                assert!(home.is_multiple_of(8));
                let root = &roots[home as usize / 8];
                assert_eq!(root["kind"], "spillSlot");
                assert_eq!(root["index"].as_u64(), Some(u64::from(home / 8)));
            }
        }
        if object || !extra_realm {
            let operand_homes = native_literal_operand_homes(code, start as usize, end as usize);
            // Both numeric/tagged alternatives may read an operand again,
            // but every dynamic native read belongs to the initialized roots.
            assert!(operand_homes.len() >= dynamic_inputs);
            for home in operand_homes {
                assert_eq!(roots[home]["kind"], "spillSlot");
                assert_eq!(roots[home]["index"].as_u64(), Some(home as u64));
            }
        }
        if !object && !extra_realm {
            assert!(
                relocations.iter().any(|row| {
                    row["target"]["kind"] == "gcCageBase"
                        && row["startOffset"]
                            .as_u64()
                            .is_some_and(|offset| start <= offset && offset < end)
                }),
                "two-cell fit compresses slab handle using the typed cage base"
            );
        }
    }
    counts.sort_unstable();
    assert_eq!(
        counts,
        [2, 4, 4, 5],
        "shaped/tagged/holey/packed native operands"
    );
    last_loop_pc
}

#[cfg(target_arch = "aarch64")]
fn native_literal_operand_homes(code: &[u8], start: usize, end: usize) -> Vec<usize> {
    let words: Vec<_> = code[start..end]
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
        .collect();
    assert!(
        words.iter().any(|&word| {
            let base = (word >> 5) & 31;
            word & 0xffff_fc1f == 0xf900_0010 && ![16, 20, 21, 31].contains(&base)
        }),
        "own LAB fit initializes its header before cursor publication"
    );
    words
        .iter()
        // The committed miss pushes its boxed packet below the body
        // homes. Its SP-relative offsets include that packet; only
        // the pre-effect fit reads use the unadjusted body SP here.
        .take_while(|&&word| word & 0xff80_03ff != 0xd100_03ff)
        .filter_map(|&word| {
            (word & 0xffc0_03ff == 0xf940_03f0).then_some(((word >> 10) & 4095) as usize)
        })
        .collect()
}

#[cfg(target_arch = "x86_64")]
fn native_literal_operand_homes(code: &[u8], start: usize, end: usize) -> Vec<usize> {
    let instructions = native_code::decode(code, start, end);
    assert!(
        instructions.iter().any(|instruction| {
            instruction.opcode() == Opcode::MOV
                && instruction.operand(1)
                    == (Operand::Register {
                        reg: RegSpec::r10(),
                    })
                && native_code::base_offset(instruction.operand(0)).is_some_and(
                    |(base, displacement)| {
                        displacement == 0
                            && base.class() == RegSpec::rax().class()
                            && ![4, 5, 10, 11, 13, 14, 15].contains(&base.num())
                    },
                )
        }),
        "own LAB fit initializes header before cursor publication"
    );
    instructions
        .iter()
        // The committed miss's boxed packet adjusts SP; only fit reads use
        // unadjusted canonical body homes.
        .take_while(|instruction| {
            !(instruction.opcode() == Opcode::SUB
                && instruction.operand(0)
                    == (Operand::Register {
                        reg: RegSpec::rsp(),
                    }))
        })
        .filter_map(|instruction| {
            if instruction.opcode() != Opcode::MOV
                || instruction.operand(0)
                    != (Operand::Register {
                        reg: RegSpec::r10(),
                    })
            {
                return None;
            }
            native_code::base_offset(instruction.operand(1))
                .filter(|(base, displacement)| {
                    *base == RegSpec::rsp() && *displacement >= 0 && displacement % 8 == 0
                })
                .map(|(_, displacement)| displacement as usize / 8)
        })
        .collect()
}

fn nonempty_probe(extra_realm: bool) {
    let stress = std::env::var("OTTER_GC_STRESS").is_ok_and(|value| value.trim() != "0");
    let collecting_calls = if stress { 512 } else { 24_576 };
    let source = nonempty_setup(collecting_calls);
    let expected = format!(
        "[[32768,32768,32768,32768],32768,{}]",
        collecting_calls * 16
    );
    let mut oracle = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .unwrap();
    let oracle_realm = extra_realm.then(|| oracle.create_realm().unwrap());
    run(&mut oracle, oracle_realm, &source, NONEMPTY_MODULE);
    let oracle_result = run(
        &mut oracle,
        oracle_realm,
        NONEMPTY_PROBE,
        "native-nonempty-probe.js",
    );
    assert_eq!(oracle_result.completion_string(), expected);
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let realm = extra_realm.then(|| runtime.create_realm().unwrap());
    run(&mut runtime, realm, &source, NONEMPTY_MODULE);
    let before = runtime.execution_stats();
    let result = run(
        &mut runtime,
        realm,
        NONEMPTY_PROBE,
        "native-nonempty-probe.js",
    );
    let after = runtime.execution_stats();
    preserve_probe(&result, "nonempty", extra_realm);
    assert_eq!(
        result.completion_string(),
        oracle_result.completion_string()
    );
    assert!(after.jit_optimized_osr_entries > before.jit_optimized_osr_entries);
    assert!(after.gc_minor_cycles > before.gc_minor_cycles);
    assert!(after.gc_minor_slot_updates - before.gc_minor_slot_updates >= 2);
    assert!(after.jit_reentrant_stub_transitions > before.jit_reentrant_stub_transitions);
    let own: Vec<_> = result
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .filter(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == NONEMPTY_MODULE
                && manifest.function_name() == NONEMPTY_FUNCTION
                && manifest.tier() == JitDebugTier::Optimizing
                && matches!(manifest.entry(), JitDebugTarget::Osr { .. })
        })
        .collect();
    assert!(
        !own.is_empty(),
        "once-called literal subject owns actual native Graph OSR"
    );
    let function_id = own[0].manifest().function_id();
    let last_loop_pc = own
        .iter()
        .map(|bundle| assert_native_nonempty_artifact(bundle, extra_realm))
        .max()
        .unwrap();
    let report = result.jit_debug_report().unwrap();
    assert!(!report.truncated());
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
                "own literal Graph leaves before collecting phase: {event:?}"
            );
            assert_eq!(reason, ExitReason::InsufficientFeedback, "{event:?}");
        }
    }
    drop(result);
    runtime
        .force_gc()
        .expect("full collection after native return");
    let retained = run(
        &mut runtime,
        realm,
        "const kept = sink[15].keep; JSON.stringify([kept.marker, kept.keep === input, kept.tagged[0] === kept, kept.tagged[3] === kept, kept.tagged[2] === input, !(1 in kept.tagged), !(1 in kept.holey), kept.packed[0] !== kept.packed[0], 1 / kept.packed[1] === -Infinity, 1 / kept.holey[2] === -Infinity, kept.packed[2] === Infinity, kept.packed[3] === -Infinity, kept.packed[4] === 1.5, Object.getPrototypeOf(kept) === Object.prototype, Object.getPrototypeOf(kept.tagged) === Array.prototype, Object.getPrototypeOf(kept.packed) === Array.prototype, Object.getPrototypeOf(kept.holey) === Array.prototype]);",
        "native-nonempty-retained.js",
    );
    assert_eq!(
        retained.completion_string(),
        "[7,true,true,true,true,true,true,true,true,true,true,true,true,true,true,true,true]"
    );
}

#[test]
fn graph_nonempty_literals_survive_real_osr_and_moving_gc() {
    nonempty_probe(false);
}

#[test]
fn graph_nonempty_literals_keep_their_additional_source_realm() {
    nonempty_probe(true);
}
