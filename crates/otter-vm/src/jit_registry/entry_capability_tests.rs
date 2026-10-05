//! Cold callable-capability and current-function selection proofs.
//!
//! # Contents
//! - Distinct retained tiers, current selection and invalid tombstone lifetime.
//! - Missing, before-mapping and one-past-end callable entries fail closed.
//! - OSR-only mappings cannot claim current ordinary entry selection.
//!
//! # Invariants
//! These metadata fixtures never execute their byte buffers. Native emitted
//! entry offsets and actual suspended execution are tested separately by the
//! JIT artifact and constructor moving-collection fixtures.
//!
//! # See also
//! - `super::retained_call_entry_offset` derives the only mapping-relative fact.
//! - `crate::native_abi::FunctionEntryCell` owns current selection.

use super::*;
use crate::native_abi::CodeObjectMetadata;

#[derive(Debug)]
struct EntryCode {
    id: u64,
    tier: NativeFrameKind,
    bytes: Box<[u8; 64]>,
    call_offset: Option<i64>,
    visible_base: bool,
    osr_only: bool,
}

impl EntryCode {
    fn new(id: u64, tier: NativeFrameKind, offset: Option<i64>) -> Self {
        Self {
            id,
            tier,
            bytes: Box::new([0; 64]),
            call_offset: offset,
            visible_base: true,
            osr_only: false,
        }
    }
}

impl JitFunctionCode for EntryCode {
    fn metadata(&self) -> CodeObjectMetadata {
        CodeObjectMetadata {
            id: self.id,
            code_block_id: 7,
            entry_offset: 0,
            code_size: 64,
            safepoint_count: 0,
            frame_map_count: 0,
            spill_map_count: 0,
            dependency_count: 0,
        }
    }
    fn native_frame_kind(&self) -> NativeFrameKind {
        self.tier
    }
    fn code_len(&self) -> usize {
        self.bytes.len()
    }
    fn entry_addr(&self) -> Option<usize> {
        Some(self.bytes.as_ptr() as usize)
    }
    fn call_entry_addr(&self) -> Option<usize> {
        let address = (self.bytes.as_ptr() as u64).checked_add_signed(self.call_offset?)?;
        usize::try_from(address).ok()
    }
    fn native_code_address(&self) -> Option<u64> {
        self.visible_base.then_some(self.bytes.as_ptr() as u64)
    }
    fn osr_only(&self) -> bool {
        self.osr_only
    }
}

#[test]
fn callable_offsets_are_distinct_from_linkage_and_survive_only_the_retained_mapping() {
    let mut registry = JitCodeRegistry::new_boxed();
    let baseline: Arc<dyn JitFunctionCode> =
        Arc::new(EntryCode::new(101, NativeFrameKind::Baseline, Some(8)));
    registry
        .register_generation(101, baseline.clone(), 0, 1, None, Box::new([]))
        .unwrap();
    let first = registry.generation_snapshot();
    assert_eq!(first.len(), 1);
    assert!(first[0].current_entry && first[0].linked);
    assert_eq!(first[0].call_entry_offset, Some(8));

    let optimizing: Arc<dyn JitFunctionCode> =
        Arc::new(EntryCode::new(102, NativeFrameKind::Optimizing, Some(16)));
    registry
        .register_generation(102, optimizing.clone(), 0, 1, None, Box::new([]))
        .unwrap();
    let both = registry.generation_snapshot();
    assert_eq!(both.len(), 2);
    assert_eq!(both[0].lifecycle, CodeLifetimeState::Installed);
    assert!(both[0].linked && !both[0].current_entry);
    assert_eq!(both[0].call_entry_offset, Some(8));
    assert!(both[1].linked && both[1].current_entry);
    assert_eq!(both[1].call_entry_offset, Some(16));

    registry.invalidate_code_objects([102]);
    let fallback = registry.generation_snapshot();
    assert!(fallback[0].current_entry);
    assert_eq!(fallback[1].lifecycle, CodeLifetimeState::Invalid);
    assert!(!fallback[1].linked && !fallback[1].current_entry);
    assert_eq!(fallback[1].call_entry_offset, Some(16));
    assert_eq!(
        registry.retire_unreferenced(),
        0,
        "external owner retains the mapping"
    );
    drop(optimizing);
    assert_eq!(registry.retire_unreferenced(), 1);
    let retired = registry.generation_snapshot();
    assert_eq!(retired[1].lifecycle, CodeLifetimeState::Retired);
    assert_eq!(retired[1].call_entry_offset, None);
    assert!(!retired[1].linked && !retired[1].current_entry);
    assert_eq!(retired[0].call_entry_offset, Some(8));
    drop(baseline);
}

#[test]
fn callable_offset_refuses_missing_and_out_of_mapping_addresses() {
    for (offset, expected) in [
        (None, None),
        (Some(-1), None),
        (Some(0), Some(0)),
        (Some(63), Some(63)),
        (Some(64), None),
        (Some(65), None),
    ] {
        let code = EntryCode::new(201, NativeFrameKind::Baseline, offset);
        assert_eq!(retained_call_entry_offset(&code), expected, "{offset:?}");
    }
    let mut hidden_base = EntryCode::new(202, NativeFrameKind::Baseline, Some(8));
    hidden_base.visible_base = false;
    assert_eq!(retained_call_entry_offset(&hidden_base), None);
}

#[test]
fn osr_only_emission_and_current_function_selection_are_independent_facts() {
    let mut registry = JitCodeRegistry::new_boxed();
    let mut code = EntryCode::new(301, NativeFrameKind::Baseline, Some(8));
    code.osr_only = true;
    registry
        .register_generation(301, Arc::new(code), 0, 1, None, Box::new([]))
        .unwrap();
    let snapshot = registry.generation_snapshot();
    assert_eq!(snapshot.len(), 1);
    assert!(snapshot[0].linked);
    assert!(!snapshot[0].current_entry);
    assert_eq!(snapshot[0].call_entry_offset, Some(8));
}

#[test]
fn linked_interpreter_fallback_is_not_an_emitted_callable_entry() {
    let mut registry = JitCodeRegistry::new_boxed();
    registry
        .register_generation(
            401,
            Arc::new(EntryCode::new(401, NativeFrameKind::Baseline, None)),
            0,
            1,
            None,
            Box::new([]),
        )
        .unwrap();
    let snapshot = registry.generation_snapshot();
    assert_eq!(snapshot.len(), 1);
    assert!(snapshot[0].linked && snapshot[0].current_entry);
    assert_eq!(snapshot[0].call_entry_offset, None);
}
