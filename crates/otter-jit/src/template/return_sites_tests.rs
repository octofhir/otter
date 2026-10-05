//! Independent Template return-site production and source-map proofs.
//!
//! # Contents
//! - Physical CALL/BLR bytes and sorted exact return-offset tables.
//! - Logical source zero, canonical frame windows and reserved-ID refusal.
//!
//! # Invariants
//! - These compile-only fixtures never execute encoded target addresses.
//! - Return proofs read finalized bytes and retained metadata without artifacts.
//! - Callable capability separately joins actual emitter offsets to captured
//!   bytes; an OSR compile-trigger label does not imply an OSR-only body.
//! - Runtime execution and moving-root lookup are covered by native edge tests.
//!
//! # See also
//! - `crate::return_sites` owns the compile-time recorder.
//! - `crate::entry::depth_tests` executes native child entry and anchor transfer.

use otter_bytecode::{Op, Operand};
use otter_vm::native_abi::{NO_FRAME_STATE, NO_SAFEPOINT, SafepointRecord};
use otter_vm::{JitCompileSnapshot, JitFunctionCode, jit::JitTestInstruction};

fn view(calls: bool) -> JitCompileSnapshot {
    let instructions = if calls {
        vec![
            JitTestInstruction::new(
                Op::Call,
                0,
                0,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                ],
            ),
            JitTestInstruction::new(
                Op::Call,
                1,
                12,
                vec![
                    Operand::Register(2),
                    Operand::Register(3),
                    Operand::ConstIndex(0),
                ],
            ),
            JitTestInstruction::new(Op::ReturnValue, 2, 24, vec![Operand::Register(2)]),
        ]
    } else {
        vec![
            JitTestInstruction::new(Op::LoadUndefined, 0, 0, vec![Operand::Register(0)]),
            JitTestInstruction::new(Op::ReturnValue, 1, 4, vec![Operand::Register(0)]),
        ]
    };
    JitCompileSnapshot::without_feedback(7, 0, 8, instructions)
}

fn assert_physical_call(bytes: &[u8], offset: usize) {
    #[cfg(target_arch = "aarch64")]
    {
        assert_eq!(offset % 4, 0);
        let word = u32::from_le_bytes(bytes[offset - 4..offset].try_into().unwrap());
        assert_eq!(
            word & 0xffff_fc1f,
            0xd63f_0000,
            "return offset must follow BLR"
        );
    }
    #[cfg(target_arch = "x86_64")]
    {
        assert_eq!(
            bytes[offset - 2],
            0xff,
            "return offset must follow indirect CALL"
        );
        assert_eq!((bytes[offset - 1] >> 3) & 7, 2, "ModRM must encode CALL /2");
    }
}

#[test]
fn calls_retain_physical_return_offsets_and_exact_zero_source_without_capture() {
    let code = super::compile(&view(true), 9, &crate::entry::TransitionTable::resolve())
        .expect("ordinary calls compile without artifact capture");
    let bytes = code.exact_bytes_for_test();
    let sites = code.return_sites();
    assert!(!sites.is_empty());
    assert!(
        sites
            .windows(2)
            .all(|w| w[0].native_return_offset < w[1].native_return_offset)
    );
    let mut source_pcs = std::collections::BTreeSet::new();
    for site in sites {
        let offset = site.native_return_offset as usize;
        assert!(offset >= 4 && offset <= bytes.len());
        assert_physical_call(bytes, offset);
        assert_ne!(site.safepoint_id, NO_SAFEPOINT);
        let record = code
            .safepoint_record(site.safepoint_id)
            .expect("retained source record");
        source_pcs.insert(record.call_pc);
        assert_eq!(record.frame_state, NO_FRAME_STATE);
        assert!(
            record.spill_roots.is_empty(),
            "the collector traces the window itself"
        );
        assert!(record.inline_frames.is_empty());
    }
    assert_eq!(source_pcs, [0, 1].into_iter().collect());
    assert_eq!(code.metadata().code_block_id, 7);
    assert_eq!(code.metadata().code_size as usize, bytes.len());
    assert!(code.native_code_address().is_some());
}

#[test]
fn pure_operations_do_not_manufacture_generated_return_sites() {
    let code = super::compile(&view(false), 10, &crate::entry::TransitionTable::resolve())
        .expect("pure function compiles");
    assert!(code.return_sites().is_empty());
}

#[test]
fn reserved_safepoint_id_is_refused_before_source_record_publication() {
    let mut plan = super::TemplatePlan::build(&view(true)).expect("valid operation plan");
    plan.safepoint_records
        .push(SafepointRecord::window(NO_SAFEPOINT - 1, NO_FRAME_STATE));
    let before = plan.safepoint_records.clone();
    assert!(crate::return_sites::template_source_safepoints(&mut plan).is_err());
    assert_eq!(plan.safepoint_records, before);
}

#[test]
fn captured_callable_entry_matches_retained_emission_for_template_and_graph() {
    use otter_vm::{JitArtifactFileName, JitArtifactIdentity, JitDebugTarget, JitDebugTier};
    let view = view(false);
    let transitions = crate::entry::TransitionTable::resolve();
    for tier in [JitDebugTier::Template, JitDebugTier::Optimizing] {
        // The trigger is intentionally OSR-labelled for both complete bodies.
        // It is diagnostic identity, not a substitute for callable capability.
        let request = Some(crate::artifact::ArtifactRequest {
            identity: JitArtifactIdentity {
                function_name: "callable-entry".into(),
                module: "callable-entry.js".into(),
            },
            tier,
            entry: JitDebugTarget::Osr { pc: 0 },
        });
        let (code, artifact): (Box<dyn JitFunctionCode>, _) = match tier {
            JitDebugTier::Template => {
                let output = super::compile_with_artifacts(&view, 41, &transitions, request, false)
                    .expect("complete Template body");
                (Box::new(output.code), output.artifact.unwrap())
            }
            JitDebugTier::Optimizing => {
                let output =
                    crate::graph::compile_optimized(&view, 42, &transitions, None, request, false)
                        .expect("complete Graph body");
                (Box::new(output.code), output.artifact.unwrap())
            }
            JitDebugTier::Interpreter => unreachable!("native compiler tiers only"),
        };
        assert_eq!(artifact.manifest().entry(), JitDebugTarget::Osr { pc: 0 });
        let map: serde_json::Value = serde_json::from_slice(
            artifact
                .file(JitArtifactFileName::CodeMap)
                .unwrap()
                .contents(),
        )
        .unwrap();
        let offset = map["callEntryOffset"]
            .as_u64()
            .expect("actual emitted call entry");
        let retained = (code.call_entry_addr().unwrap() as u64)
            .checked_sub(code.native_code_address().unwrap())
            .unwrap();
        assert_eq!(offset, retained);
        let bytes = artifact.file(JitArtifactFileName::Code).unwrap().contents();
        assert_eq!(bytes.len(), code.code_len());
        assert!(offset < bytes.len() as u64);
        assert_ne!(map["entryOffset"].as_u64(), Some(offset));
        #[cfg(target_arch = "aarch64")]
        assert_eq!(
            &bytes[offset as usize..offset as usize + 4],
            &0xa9bd_7bfdu32.to_le_bytes()
        );
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            &bytes[offset as usize..offset as usize + 4],
            &[0x55, 0x48, 0x89, 0xe5]
        );
    }
}
