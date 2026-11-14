//! Selective compiled-source retirement and retained constant ownership.
//!
//! # Contents
//! - Candidate source ranges and actual splices retire without unlinking callers.
//! - Invalid generations keep strong constants traced until physical retirement.
//!
//! # Invariants
//! - Fixture code owns real byte buffers but never executes them.
//! - Moving collection uses the registry's production strong and weak root walks.
//! - Invalidation changes executable admission, never an embedded constant word.
//!
//! # See also
//! - `crate::jit_roots` classifies roots that can retain JavaScript source.

use super::*;
use crate::Value;
use crate::code_space::ChunkEvictionCandidate;
use crate::jit_roots::{CalleeIdentityCell, CompilationRoot};
use crate::native_abi::CodeObjectMetadata;

#[derive(Debug)]
struct Code {
    id: u64,
    function_id: u32,
    tier: NativeFrameKind,
    spliced: Box<[u32]>,
    bytes: Box<[u8; 64]>,
}

impl JitFunctionCode for Code {
    fn metadata(&self) -> CodeObjectMetadata {
        CodeObjectMetadata {
            id: self.id,
            code_block_id: self.function_id,
            entry_offset: 0,
            code_size: self.bytes.len() as u32,
            safepoint_count: 0,
            frame_map_count: 0,
            spill_map_count: 0,
            dependency_count: 0,
        }
    }

    fn native_frame_kind(&self) -> NativeFrameKind {
        self.tier
    }

    fn spliced_functions(&self) -> &[u32] {
        &self.spliced
    }

    fn code_len(&self) -> usize {
        self.bytes.len()
    }

    fn entry_addr(&self) -> Option<usize> {
        Some(self.bytes.as_ptr() as usize)
    }

    fn call_entry_addr(&self) -> Option<usize> {
        self.entry_addr().map(|base| base + 8)
    }

    fn native_code_address(&self) -> Option<u64> {
        Some(self.bytes.as_ptr() as u64)
    }
}

fn code(id: u64, function_id: u32, tier: NativeFrameKind, spliced: &[u32]) -> Arc<Code> {
    Arc::new(Code {
        id,
        function_id,
        tier,
        spliced: spliced.into(),
        bytes: Box::new([0; 64]),
    })
}

fn candidate() -> ChunkEvictionCandidate {
    ChunkEvictionCandidate {
        function_base: 7,
        function_count: 2,
        property_ic_site_base: 0,
        property_ic_site_end: 0,
        module_identity: 1,
        retained_bytes: 1,
        alias: None,
    }
}

#[test]
fn reclamation_retires_candidate_tiers_and_splices_but_preserves_caller_entries() {
    let mut registry = JitCodeRegistry::new_boxed();
    for (id, fid, tier, spliced) in [
        (301, 7, NativeFrameKind::Baseline, &[][..]),
        (302, 7, NativeFrameKind::Optimizing, &[][..]),
        (303, 8, NativeFrameKind::Optimizing, &[][..]),
        (304, 42, NativeFrameKind::Optimizing, &[6, 8, 9][..]),
        (305, 20, NativeFrameKind::Baseline, &[][..]),
        (306, 9, NativeFrameKind::Optimizing, &[6][..]),
    ] {
        let roots: Box<[CompilationRoot]> = if id == 305 {
            Box::new([CompilationRoot::CalleeIdentity(Arc::new(
                CalleeIdentityCell::new(),
            ))])
        } else {
            Box::new([])
        };
        registry
            .register_generation(id, code(id, fid, tier, spliced), 1, 4, Some(1), roots)
            .unwrap();
    }
    let source_cell = std::ptr::from_ref(registry.function_entry_cells[&7].as_ref()) as u64;
    let caller_cell = std::ptr::from_ref(registry.function_entry_cells[&20].as_ref()) as u64;
    let caller_generation = registry.function_entry_cells[&20].current_generation();
    let caller_entry = registry.entry_cells[&305]
        .cell
        .entry_addr
        .load(std::sync::atomic::Ordering::Acquire);
    assert_ne!(caller_entry, 0);

    assert!(registry.invalidate_for_code_reclamation(&[]).is_empty());
    assert_eq!(
        registry.invalidate_for_code_reclamation(&[candidate()]),
        [7, 8, 42]
    );
    for id in 301..=304 {
        assert_eq!(registry.codes[&id].state, CodeLifetimeState::Invalid);
        assert!(!registry.is_current_generation(id));
    }
    assert_eq!(registry.retire_unreferenced(), 4);
    assert!(
        registry
            .invalidate_for_code_reclamation(&[candidate()])
            .is_empty()
    );
    for id in [305, 306] {
        assert!(registry.is_current_generation(id));
        assert_eq!(registry.codes[&id].state, CodeLifetimeState::Installed);
    }
    assert_eq!(
        std::ptr::from_ref(registry.function_entry_cells[&20].as_ref()) as u64,
        caller_cell
    );
    assert_eq!(
        registry.function_entry_cells[&20].current_generation(),
        caller_generation
    );
    assert_eq!(
        registry.entry_cells[&305]
            .cell
            .entry_addr
            .load(std::sync::atomic::Ordering::Acquire),
        caller_entry,
        "the retained caller keeps its exact callable native destination"
    );
    let (source, destination) = registry.published_function_entry(7).unwrap();
    assert_eq!(std::ptr::from_ref(source) as u64, source_cell);
    assert_eq!(destination.code_object_id, 0);
    assert_eq!(
        destination.native_frame_header.kind,
        NativeFrameKind::Interpreter
    );
}

struct RegistryRoots<'a>(&'a JitCodeRegistry);

impl otter_gc::ExtraRootSource for RegistryRoots<'_> {
    fn visit_extra_roots(
        &self,
        roots: otter_gc::RootSet,
        visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc),
    ) {
        self.0
            .trace_retained_roots(visitor, roots == otter_gc::RootSet::Young);
    }

    fn process_weak(
        &self,
        visitor: &mut dyn FnMut(otter_gc::raw::RawGc) -> Option<otter_gc::raw::RawGc>,
    ) {
        self.0.process_weak(visitor);
    }
}

#[test]
fn reclamation_keeps_invalid_generation_constants_until_physical_retirement() {
    let mut heap = otter_gc::GcHeap::new().unwrap();
    let context = crate::context::alloc_context_with_roots(
        &mut heap,
        crate::context::ContextShape {
            scope_function_id: 7,
            scope_index: 0,
            slot_count: 1,
            has_extension: false,
        },
        Value::undefined(),
        |_| false,
        &mut |_| {},
    )
    .unwrap();
    let original = Value::context(context);
    let held = Arc::new(CalleeIdentityCell::holding(original));
    let mut registry = JitCodeRegistry::new_boxed();
    let owner = code(401, 100, NativeFrameKind::Optimizing, &[]);
    registry
        .register_generation(
            401,
            owner.clone(),
            0,
            4,
            Some(1),
            Box::new([CompilationRoot::CalleeIdentity(held.clone())]),
        )
        .unwrap();
    assert_eq!(
        registry.invalidate_for_code_reclamation(&[candidate()]),
        [100],
        "a source-independent body can retain candidate source through a constant"
    );
    assert_eq!(
        held.held(),
        original,
        "invalidation must not empty a strong cell"
    );
    assert_eq!(
        registry.retire_unreferenced(),
        0,
        "an executable owner remains"
    );
    {
        let roots = RegistryRoots(&registry);
        let _guard = heap.register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        heap.collect_minor_with_roots(&mut |_| {}).unwrap();
    }
    let relocated = held.held();
    assert_ne!(
        relocated, original,
        "the invalid generation still owns moving roots"
    );
    assert_eq!(
        heap.read_payload(relocated.as_context().unwrap(), |body| body
            .scope_function_id),
        7
    );
    assert_eq!(Arc::strong_count(&held), 2);
    drop(owner);
    assert_eq!(registry.retire_unreferenced(), 1);
    assert_eq!(
        Arc::strong_count(&held),
        1,
        "physical retirement releases the root owner"
    );
    assert_eq!(
        held.held(),
        relocated,
        "retirement drops ownership without rewriting data"
    );
    let mut slots = 0;
    registry.trace_retained_roots(&mut |_| slots += 1, false);
    assert_eq!(slots, 0);
    let generation = registry.generation_snapshot().pop().unwrap();
    assert_eq!(generation.lifecycle, CodeLifetimeState::Retired);
    assert!(!generation.linked);
    assert_eq!(generation.active_count, 0);
}
