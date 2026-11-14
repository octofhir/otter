//! Rooted LAB fit preparation and exact native Array publication proofs.
//!
//! # Contents
//! - Zero-argument interpreted control admitting a fixed short-probe LAB budget.
//! - AArch64 and x86 machine proofs over the exact Array-length operation region.
//!
//! # Invariants
//! - Preparation retains the known runtime source and verifies activation roots;
//!   no unrooted local values cross its optional collection boundary.
//! - Positive external GC stress remains enabled and bypasses fit preparation.
//! - A fit publishes the context-owned LAB top without calling a helper, then
//!   branches over the exact typed allocation helper to the region end.
//! - The parent pairs these emitted-code proofs with exact zero helper hits,
//!   native trace exclusion, unchanged generations and observable Array results.
//!
//! # See also
//! - `otter_gc::GcHeap::ensure_machine_allocation_with_roots` owns admission.
//! - `otter-jit/src/graph/{arm64,x86_64}/allocation/lexical.rs` emits this path.

use super::artifact_json;
use otter_runtime::{JitArtifactBundle, JitArtifactFileName};
use std::sync::Arc;

#[cfg(target_arch = "x86_64")]
#[path = "../support/native_code.rs"]
mod native_code;

pub(super) fn fit_preparation_call() -> otter_runtime::RuntimeNativeCall {
    use otter_runtime::{
        RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError, RuntimeValue, ValueWindow,
    };

    RuntimeNativeCall::Dynamic(Arc::new(
        |ctx: &mut RuntimeNativeCtx<'_>, args: ValueWindow<'_>, _state: ValueWindow<'_>| {
            let vm = ctx.interp_mut();
            let runtime_roots = otter_gc::ExtraRoots::new(&*vm);
            let heap = vm.gc_heap_mut();
            if !args.is_empty() || !heap.has_frame_root_providers() {
                return Err(RuntimeNativeError::Error {
                    message: "fit preparation requires its zero-argument rooted dispatch".into(),
                });
            }
            // ExtraRoots identity is a conservative deduplication query: an
            // optimized cross-crate thunk can differ from exec::run's thunk.
            // Keep the actual Interpreter source registered for this whole
            // scope, following Interpreter::scope_runtime_roots_guard. A
            // duplicate source's collection hooks and visits are idempotent.
            let _runtime_roots = if heap.has_extra_root_source(runtime_roots) {
                None
            } else {
                Some(heap.register_extra_roots(runtime_roots))
            };
            if !heap.has_extra_root_source(runtime_roots) {
                return Err(RuntimeNativeError::Error {
                    message: "fit preparation requires its known runtime root source".into(),
                });
            }
            if !heap.gc_stress_enabled() {
                // This interpreted control runs after script preparation and
                // before the tested calls. NativeCtx owns the rooted activation
                // turn and the known runtime source remains registered; no
                // Rust-local GC values need an additional external visitor.
                // Four KiB exceeds all cells in each fixed, short fit probe.
                const FIT_BYTES: usize = 4096;
                let admitted = heap
                    .ensure_machine_allocation_with_roots(FIT_BYTES, &mut |_| {})
                    .map_err(|error| RuntimeNativeError::Error {
                        message: error.to_string(),
                    })?;
                if !admitted || !heap.machine_allocation_ready(FIT_BYTES) {
                    return Err(RuntimeNativeError::Error {
                        message: "fit probe requires an admitted four-KiB LAB window".into(),
                    });
                }
            }
            Ok(RuntimeValue::undefined())
        },
    ))
}

pub(super) fn assert_array_length_lab_publication(
    bundle: &JitArtifactBundle,
    region: &serde_json::Value,
) {
    let start = region["startOffset"].as_u64().unwrap() as usize;
    let end = region["endOffset"].as_u64().unwrap() as usize;
    let relocations = artifact_json(bundle, JitArtifactFileName::Relocations);
    let helpers: Vec<_> = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|relocation| {
            relocation["target"]["name"] == "array_construct_alloc"
                && relocation["startOffset"]
                    .as_u64()
                    .is_some_and(|offset| (start..end).contains(&(offset as usize)))
        })
        .collect();
    assert_eq!(helpers.len(), 1, "one collecting typed slow boundary");
    let helper = helpers[0]["startOffset"].as_u64().unwrap() as usize;
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let lab_offset = std::mem::offset_of!(otter_vm::native_abi::JitCtx, alloc_window)
        + std::mem::offset_of!(otter_vm::jit::JitMachineAllocationWindow, lab);
    let top_offset = otter_vm::jit::JIT_LAB_TOP_OFFSET;

    // The publication store is immediately preceded by reloading the LAB.
    // Its successor returns the initialized candidate and skips the exact
    // collecting helper. Combined with zero helper hits and no interpreter
    // dispatch, this proves a native fit rather than just an allocation node.
    #[cfg(target_arch = "aarch64")]
    {
        assert_eq!((start % 4, end % 4), (0, 0));
        let words: Vec<_> = code[start..end]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_le_bytes(*bytes))
            .collect();
        let load_lab = 0xf940_0000 | ((lab_offset as u32 / 8) << 10) | (20 << 5);
        let publication = words
            .windows(2)
            .position(|pair| {
                pair[0] & !31 == load_lab
                    && pair[1] & !31
                        == 0xf900_0000 | ((top_offset / 8) << 10) | ((pair[0] & 31) << 5)
            })
            .expect("the emitted fit publishes the LAB top");
        assert!(start + (publication + 1) * 4 < helper);
        assert!(
            words[..=publication + 1].iter().all(|word| {
                word & 0xfc00_0000 != 0x9400_0000 && word & 0xffff_fc1f != 0xd63f_0000
            }),
            "the fit has no native call before publication"
        );
        assert!(
            (publication + 2..=publication + 3).any(|index| {
                let word = words[index];
                let displacement = ((word << 6) as i32 >> 6) as isize * 4;
                word & 0xfc00_0000 == 0x1400_0000
                    && (start + index * 4) as isize + displacement == end as isize
            }),
            "the fit skips the typed helper and returns the candidate"
        );
    }
    #[cfg(target_arch = "x86_64")]
    {
        use yaxpeax_arch::LengthedInstruction;
        use yaxpeax_x86::amd64::{Opcode, Operand, RegSpec};

        let instructions = native_code::decode(code, start, end);
        let publication = instructions
            .windows(2)
            .position(|pair| {
                let Operand::Register { reg: buffer } = pair[0].operand(0) else {
                    return false;
                };
                pair[0].opcode() == Opcode::MOV
                    && pair[0].mem_size().and_then(|size| size.bytes_size()) == Some(8)
                    && native_code::base_offset(pair[0].operand(1))
                        == Some((RegSpec::r15(), lab_offset as i32))
                    && pair[1].opcode() == Opcode::MOV
                    && pair[1].mem_size().and_then(|size| size.bytes_size()) == Some(8)
                    && matches!(pair[1].operand(1), Operand::Register { .. })
                    && native_code::base_offset(pair[1].operand(0))
                        == Some((buffer, top_offset as i32))
            })
            .expect("the emitted fit publishes the LAB top");
        assert!(
            instructions[..=publication + 1]
                .iter()
                .all(|instruction| instruction.opcode() != Opcode::CALL)
        );
        let mut offset = start;
        let mut skips_helper = false;
        for (index, instruction) in instructions.iter().enumerate() {
            offset += instruction.len().to_const() as usize;
            if index == publication + 1 {
                assert!(offset <= helper);
            }
            if (publication + 2..=publication + 3).contains(&index)
                && instruction.opcode() == Opcode::JMP
            {
                let displacement = match instruction.operand(0) {
                    Operand::ImmediateI8 { imm } => isize::from(imm),
                    Operand::ImmediateI32 { imm } => imm as isize,
                    operand => panic!("expected a local fit continuation, got {operand:?}"),
                };
                skips_helper |= offset as isize + displacement == end as isize;
            }
        }
        assert!(skips_helper, "the fit skips the typed helper");
    }
}
