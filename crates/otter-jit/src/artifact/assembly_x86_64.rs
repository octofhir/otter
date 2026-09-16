//! Deterministic, address-redacted x86-64 assembly artifacts.
//!
//! # Contents
//! - [`render`] decodes finalized `code.bin` bytes into an offset-based text
//!   listing.
//! - Relocation ranges are rendered symbolically without their process-local
//!   immediate bytes.
//!
//! # Invariants
//! - Every location is relative to the matching `code.bin`.
//! - No relocation immediate or absolute process address is printed.
//! - Decoder failures preserve exact bytes through a one-byte fallback.
//!
//! # See also
//! - [`super::relocation`] for target-neutral symbolic relocation identities.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

use otter_vm::{JitArtifactMetadata, deopt::DeoptTable, native_abi::SafepointRecord};
use yaxpeax_arch::LengthedInstruction;
use yaxpeax_x86::amd64::InstDecoder;

use super::CodeMapCapture;
use super::relocation::{ValidatedRelocation, ValidatedRelocations};

pub(super) fn render(
    metadata: &JitArtifactMetadata,
    code: &[u8],
    entry_offset: usize,
    code_map: &CodeMapCapture,
    relocations: &ValidatedRelocations,
    deopt_table: Option<&DeoptTable>,
    safepoints: &[SafepointRecord],
) -> String {
    let mut output = String::with_capacity(code.len().saturating_mul(24));
    output.push_str("; otter jit x86-64 assembly\n");
    output.push_str("; offset-basis=code.bin\n");
    writeln!(output, "; target={}", metadata.target).expect("String write");
    writeln!(output, "; architecture={}", metadata.architecture).expect("String write");
    writeln!(output, "; operating-system={}", metadata.operating_system).expect("String write");
    writeln!(output, "; tier={:?}", metadata.tier).expect("String write");
    writeln!(output, "; function-id={}", metadata.function_id).expect("String write");
    writeln!(output, "; function-name={:?}", metadata.function_name).expect("String write");
    writeln!(output, "; module={:?}", metadata.module).expect("String write");
    writeln!(output, "; code-object-id={}", metadata.code_object_id).expect("String write");
    writeln!(output, "; compile-target={:?}", metadata.entry).expect("String write");
    writeln!(output, "; entry-offset=+0x{entry_offset:08x}").expect("String write");
    writeln!(output, "; code-bytes={}", code.len()).expect("String write");
    writeln!(
        output,
        "; deopt-exits={} safepoints={}",
        deopt_table.map_or(0, DeoptTable::len),
        safepoints.len()
    )
    .expect("String write");
    output.push('\n');

    let decoder = InstDecoder::default();
    let labels = collect_labels(code, entry_offset, code_map, relocations, &decoder);
    let region_starts = region_starts(code_map);
    let osr_starts = osr_starts(code_map);
    let mut relocation_index = 0usize;
    let mut offset = 0usize;
    while offset < code.len() {
        if labels.contains(&offset) {
            writeln!(output, "L{offset:08x}:").expect("String write");
        }
        if let Some(entries) = osr_starts.get(&offset) {
            for entry in entries {
                writeln!(
                    output,
                    "  ; osr-entry pc={} range=+0x{:08x}..+0x{:08x}",
                    entry.logical_pc, entry.start_offset, entry.end_offset
                )
                .expect("String write");
            }
        }
        if let Some(regions) = region_starts.get(&offset) {
            for region in regions {
                render_region(&mut output, region);
            }
        }
        if let Some(relocation) = relocations.records.get(relocation_index)
            && relocation.start_offset as usize == offset
        {
            render_relocation(&mut output, relocation);
            offset = relocation.end_offset as usize;
            relocation_index += 1;
            continue;
        }
        match decoder.decode_slice(&code[offset..]) {
            Ok(instruction) => {
                let len = usize::try_from(instruction.len().to_const())
                    .unwrap_or(1)
                    .max(1)
                    .min(code.len() - offset);
                let bytes = code[offset..offset + len]
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                let rendered = decode_direct_branch(&code[offset..offset + len], offset)
                    .filter(|branch| branch.target < code.len())
                    .map_or_else(|| instruction.to_string(), render_branch);
                writeln!(output, "+0x{offset:08x}: {bytes:<30}  {rendered}").expect("String write");
                offset += len;
            }
            Err(_) => {
                writeln!(
                    output,
                    "+0x{offset:08x}: {:02x}                              .byte 0x{:02x}",
                    code[offset], code[offset]
                )
                .expect("String write");
                offset += 1;
            }
        }
    }
    output
}

fn region_starts(code_map: &CodeMapCapture) -> BTreeMap<usize, Vec<&super::CodeRegion>> {
    let mut starts = BTreeMap::<usize, Vec<&super::CodeRegion>>::new();
    for region in &code_map.regions {
        starts
            .entry(region.start_offset as usize)
            .or_default()
            .push(region);
    }
    for regions in starts.values_mut() {
        regions.sort_by_key(|region| (region.end_offset, region.kind, region.operation_index));
    }
    starts
}

fn osr_starts(code_map: &CodeMapCapture) -> BTreeMap<usize, Vec<&super::OsrCodeEntry>> {
    let mut starts = BTreeMap::<usize, Vec<&super::OsrCodeEntry>>::new();
    for entry in &code_map.osr_entries {
        starts
            .entry(entry.start_offset as usize)
            .or_default()
            .push(entry);
    }
    for entries in starts.values_mut() {
        entries.sort_by_key(|entry| (entry.logical_pc, entry.end_offset));
    }
    starts
}

fn render_region(output: &mut String, region: &super::CodeRegion) {
    write!(
        output,
        "  ; region kind={} range=+0x{:08x}..+0x{:08x}",
        region.kind, region.start_offset, region.end_offset
    )
    .expect("String write");
    if let Some(block) = region.block {
        write!(output, " block={block}").expect("String write");
    }
    if let Some(target_block) = region.target_block {
        write!(output, " target-block={target_block}").expect("String write");
    }
    if let Some(function_id) = region.function_id {
        write!(output, " function={function_id}").expect("String write");
    }
    if let Some(logical_pc) = region.logical_pc {
        write!(output, " pc={logical_pc}").expect("String write");
    }
    if let Some(byte_pc) = region.byte_pc {
        write!(output, " byte-pc={byte_pc}").expect("String write");
    }
    if let Some(operation_index) = region.operation_index {
        write!(output, " operation-index={operation_index}").expect("String write");
    }
    if let Some(operation) = &region.operation {
        write!(output, " tier-op={operation:?}").expect("String write");
    }
    if let Some(exit_id) = region.deopt_exit_id {
        write!(output, " deopt-exit={exit_id}").expect("String write");
    }
    output.push('\n');
}

fn collect_labels(
    code: &[u8],
    entry_offset: usize,
    code_map: &CodeMapCapture,
    relocations: &ValidatedRelocations,
    decoder: &InstDecoder,
) -> BTreeSet<usize> {
    let mut labels = BTreeSet::new();
    if entry_offset < code.len() {
        labels.insert(entry_offset);
    }
    for entry in &code_map.osr_entries {
        if entry.start_offset < code.len() as u64 {
            labels.insert(entry.start_offset as usize);
        }
    }
    let mut relocation_index = 0usize;
    let mut offset = 0usize;
    while offset < code.len() {
        if let Some(relocation) = relocations.records.get(relocation_index)
            && relocation.start_offset as usize == offset
        {
            offset = relocation.end_offset as usize;
            relocation_index += 1;
            continue;
        }
        let Ok(instruction) = decoder.decode_slice(&code[offset..]) else {
            offset += 1;
            continue;
        };
        let len = usize::try_from(instruction.len().to_const())
            .unwrap_or(1)
            .max(1)
            .min(code.len() - offset);
        if let Some(branch) = decode_direct_branch(&code[offset..offset + len], offset)
            && branch.target < code.len()
        {
            labels.insert(branch.target);
        }
        offset += len;
    }
    labels
}

#[derive(Clone, Copy)]
struct DirectBranch {
    target: usize,
    mnemonic: &'static str,
}

fn decode_direct_branch(bytes: &[u8], offset: usize) -> Option<DirectBranch> {
    let (mnemonic, displacement) = match bytes {
        [0xe8, rest @ ..] if rest.len() == 4 => ("call", read_i32(rest)? as i64),
        [0xe9, rest @ ..] if rest.len() == 4 => ("jmp", read_i32(rest)? as i64),
        [0xeb, displacement] => ("jmp", i64::from(*displacement as i8)),
        [opcode @ 0x70..=0x7f, displacement] => (
            condition_name(*opcode & 0x0f),
            i64::from(*displacement as i8),
        ),
        [0x0f, opcode @ 0x80..=0x8f, rest @ ..] if rest.len() == 4 => {
            (condition_name(*opcode & 0x0f), read_i32(rest)? as i64)
        }
        [opcode @ 0xe0..=0xe3, displacement] => {
            (loop_name(*opcode), i64::from(*displacement as i8))
        }
        _ => return None,
    };
    let next = offset.checked_add(bytes.len())?;
    let target = i64::try_from(next).ok()?.checked_add(displacement)?;
    Some(DirectBranch {
        target: usize::try_from(target).ok()?,
        mnemonic,
    })
}

fn read_i32(bytes: &[u8]) -> Option<i32> {
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}

fn condition_name(condition: u8) -> &'static str {
    const NAMES: [&str; 16] = [
        "jo", "jno", "jb", "jae", "je", "jne", "jbe", "ja", "js", "jns", "jp", "jnp", "jl", "jge",
        "jle", "jg",
    ];
    NAMES[usize::from(condition)]
}

fn loop_name(opcode: u8) -> &'static str {
    match opcode {
        0xe0 => "loopne",
        0xe1 => "loope",
        0xe2 => "loop",
        0xe3 => "jrcxz",
        _ => unreachable!("validated loop opcode"),
    }
}

fn render_branch(branch: DirectBranch) -> String {
    format!("{} L{:08x}", branch.mnemonic, branch.target)
}

fn render_relocation(output: &mut String, relocation: &ValidatedRelocation) {
    let bytes = relocation.end_offset - relocation.start_offset;
    writeln!(
        output,
        "+0x{:08x}: relocation r{}, {:?} ; encoded-bytes={bytes} redacted",
        relocation.start_offset, relocation.register, relocation.target
    )
    .expect("String write");
}
