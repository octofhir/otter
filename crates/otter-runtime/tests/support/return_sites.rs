//! Exact native JS return sites joined to their source-owned collector records.
//!
//! # Contents
//! - Strict current-format table validation over captured executable bytes.
//! - Known function-cell CALL/BLR offsets joined to their instruction regions.
//!
//! # Invariants
//! Metadata alone cannot prove execution: callers independently establish the
//! exact installed generation and absent interpreter dispatch during the probe.
//! Every registered return follows an actual machine call and names one source
//! record. A tail JMP/BR never manufactures a return site.
//!
//! # See also
//! - `jit_actual_only_calls` checks live moving aliases and suspended source.
//! - `jit_template_direct_calls` checks exact entry deltas and linkage.

use otter_runtime::{JitArtifactBundle, JitArtifactFileName};
use serde_json::Value;

fn json(bundle: &JitArtifactBundle, name: JitArtifactFileName) -> Value {
    serde_json::from_slice(bundle.file(name).unwrap().contents()).unwrap()
}

/// Check every association and the actual call ending at its return offset.
pub(super) fn assert_sites(bundle: &JitArtifactBundle) -> Value {
    let metadata = json(bundle, JitArtifactFileName::Safepoints);
    let records = metadata["records"].as_array().unwrap();
    let sites = metadata["returnSites"].as_array().unwrap();
    assert!(
        !sites.is_empty(),
        "{} owns generated JS calls",
        bundle.manifest().function_name()
    );
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().unwrap();
    let mut previous = 0;
    for site in sites {
        let offset = site["nativeReturnOffset"].as_u64().unwrap() as usize;
        assert!(
            previous < offset && offset < code.len(),
            "strict complete-mapping offsets"
        );
        previous = offset;
        let id = site["safepointId"].as_u64().unwrap();
        let matching: Vec<_> = records
            .iter()
            .filter(|record| record["id"].as_u64() == Some(id))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "one source/roots owner for actual return"
        );
        let record = matching[0];
        assert!(record["callPc"].as_u64().is_some());
        assert!(
            record["taggedLocations"]
                .as_array()
                .unwrap()
                .iter()
                .all(|location| location["kind"] != "machineRegister")
        );
        let owners: Vec<_> = regions
            .iter()
            .filter(|region| {
                (region["kind"] == "instruction" || region["kind"] == "out-of-line")
                    && region["startOffset"]
                        .as_u64()
                        .is_some_and(|start| start < offset as u64)
                    && region["endOffset"]
                        .as_u64()
                        .is_some_and(|end| offset as u64 <= end)
            })
            .collect();
        assert_eq!(owners.len(), 1, "actual call has one emitted region");
        #[cfg(target_arch = "aarch64")]
        {
            assert!(offset >= 4 && offset % 4 == 0);
            let word = u32::from_le_bytes(code[offset - 4..offset].try_into().unwrap());
            assert!(
                word & 0xffff_fc1f == 0xd63f_0000 || word & 0xfc00_0000 == 0x9400_0000,
                "exact return must follow BLR/BL: {word:08x}"
            );
        }
        #[cfg(target_arch = "x86_64")]
        {
            use yaxpeax_arch::LengthedInstruction;
            use yaxpeax_x86::amd64::{InstDecoder, Opcode};
            let mut cursor = owners[0]["startOffset"].as_u64().unwrap() as usize;
            let decoder = InstDecoder::default();
            let mut last = None;
            while cursor < offset {
                let instruction = decoder.decode_slice(&code[cursor..offset]).unwrap();
                let length = instruction.len().to_const() as usize;
                assert!(length > 0 && cursor + length <= offset);
                cursor += length;
                last = Some(instruction.opcode());
            }
            assert_eq!(cursor, offset, "return is an exact instruction boundary");
            assert_eq!(
                last,
                Some(Opcode::CALL),
                "registered return follows the CALL itself, before result cleanup"
            );
        }
    }
    metadata
}

/// Join real Known-call bytes, exact callee identity, source and table entry.
pub(super) fn assert_known(bundle: &JitArtifactBundle, callee: u32) {
    let metadata = assert_sites(bundle);
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let mut proved = 0;
    for relocation in relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|relocation| {
            relocation["target"]["kind"] == "functionEntryCell"
                && relocation["target"]["functionId"].as_u64() == Some(u64::from(callee))
        })
    {
        let end = relocation["endOffset"].as_u64().unwrap() as usize;
        #[cfg(target_arch = "aarch64")]
        let sequence: &[u8] = &[
            0x08, 0x01, 0x40, 0xf9, 0x10, 0x01, 0x40, 0xf9, 0x00, 0x02, 0x3f, 0xd6,
        ];
        #[cfg(target_arch = "x86_64")]
        let sequence: &[u8] = &[0x4d, 0x8b, 0x0b, 0x41, 0xff, 0x11];
        // Graph also reads entry-cell admission resources at other relocations.
        // Only this actual private JS call sequence proves a return association.
        if code.get(end..end + sequence.len()) != Some(sequence) {
            continue;
        }
        let offset = end + sequence.len();
        let sites: Vec<_> = metadata["returnSites"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|site| site["nativeReturnOffset"].as_u64() == Some(offset as u64))
            .collect();
        assert_eq!(
            sites.len(),
            1,
            "this exact Known CALL owns its one return association"
        );
        let record = metadata["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["id"] == sites[0]["safepointId"])
            .unwrap();
        let region = map["regions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|region| {
                region["kind"] == "instruction"
                    && region["startOffset"]
                        .as_u64()
                        .is_some_and(|start| start <= end as u64)
                    && region["endOffset"]
                        .as_u64()
                        .is_some_and(|finish| offset as u64 <= finish)
            })
            .unwrap();
        assert_eq!(
            region["functionId"].as_u64(),
            Some(u64::from(bundle.manifest().function_id()))
        );
        assert_eq!(
            record["callPc"], region["logicalPc"],
            "same source instruction owns CALL and roots"
        );
        assert!(region["bytePc"].as_u64().is_some());
        let operation = region["operation"].as_str().unwrap();
        if operation.contains("CallJs")
            || operation.starts_with("Call {")
            || operation.starts_with("CallWithThis {")
        {
            assert_plain_hot_source_unstamped(
                code,
                region["startOffset"].as_u64().unwrap() as usize,
                offset,
            );
        }
        proved += 1;
    }
    assert!(
        proved > 0,
        "{} -> {callee} has actual Known-call bytes, not just a relocation",
        bundle.manifest().function_name()
    );
}

/// The controlled plain Known-call source contains its guards, staging and
/// actual CALL; the callee's separate prologue is outside this source region.
/// Tail layouts can place a collecting outgrown branch before the ordinary
/// fallback CALL, so they use the separate source/retired-frame execution proof.
pub(super) fn assert_plain_hot_source_unstamped(code: &[u8], start: usize, end: usize) {
    let offsets = [
        (std::mem::offset_of!(otter_vm::native_abi::Frame, header)
            + std::mem::offset_of!(otter_vm::native_abi::VmFrameHeader, pc)) as u32,
        otter_vm::native_abi::NATIVE_FRAME_CALL_SITE_OFFSET,
    ];
    #[cfg(target_arch = "aarch64")]
    {
        assert!(start % 4 == 0 && end % 4 == 0);
        for word in code[start..end].chunks_exact(4) {
            let word = u32::from_le_bytes(word.try_into().unwrap());
            // STR Wt,[x21,#imm12*4] is the actual shared caller-stamp form.
            let is_frame_store = word & 0xffc0_03e0 == 0xb900_02a0;
            let offset = ((word >> 10) & 0xfff) * 4;
            assert!(
                !is_frame_store || !offsets.contains(&offset),
                "plain hot JS CALL contains a caller PC/root stamp: {word:08x}"
            );
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        use yaxpeax_arch::LengthedInstruction;
        use yaxpeax_x86::amd64::{InstDecoder, Opcode, Operand, RegSpec};
        let decoder = InstDecoder::default();
        let mut cursor = start;
        while cursor < end {
            let instruction = decoder.decode_slice(&code[cursor..end]).unwrap();
            let length = instruction.len().to_const() as usize;
            assert!(length > 0 && cursor + length <= end);
            let destination = match instruction.operand(0) {
                Operand::MemDeref { base } => Some((base, 0)),
                Operand::Disp { base, disp } => Some((base, disp)),
                _ => None,
            };
            if instruction.opcode() == Opcode::MOV {
                assert!(
                    !destination.is_some_and(|(base, offset)| base == RegSpec::r14()
                        && offsets
                            .iter()
                            .any(|expected| i64::from(*expected) == i64::from(offset))),
                    "plain hot JS CALL stamps caller state at +{cursor:#x}: {instruction}"
                );
            }
            cursor += length;
        }
        assert_eq!(cursor, end);
    }
}
