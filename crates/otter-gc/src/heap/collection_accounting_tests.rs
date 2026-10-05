//! Collector commits for the sole physical-cell and backing-store cap ledger.
//!
//! # Contents
//! - Minor copy/promotion refunds only doomed nursery cells and released tokens.
//! - Full, split and incremental sweep commit surviving old/LOS/young bytes.
//! - Diagnostic cells charge once while an ordinary live LAB retains its tail.
//! - Cap-disabled collection preserves reservation release without cell charging.
//!
//! # Invariants
//! - Exact footprints, aliases and marker values survive real collecting paths.
//! - Explicit collection makes reclaimed capacity immediately available without
//!   another emergency collection; full-cap refusal still books no bytes.
//! - External token destruction and physical cell retirement are separate debts.
//!
//! # See also
//! - `super::GcHeap::collect_minor_work` commits successful nursery evacuation.
//! - `super::GcHeap::sweep_phase_with_pause_start` commits completed full sweep.

use super::*;
use crate::HandleScope;
use crate::test_support::{OpaqueLeaf, OpaquePair};

struct Backing {
    _external: ExternalMemory,
    child: RawGc,
}

impl Traceable for Backing {
    const TYPE_TAG: u8 = 0xeb;

    unsafe fn trace_slots(this: *mut Self, visitor: &mut crate::trace::SlotVisitor<'_>) {
        // SAFETY: the collector supplies this registered live/pending body.
        unsafe { visitor(std::ptr::addr_of_mut!((*this).child)) };
    }
}

fn bytes<T>(extra: usize) -> u64 {
    align_up(
        std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>() + extra,
        CELL_SIZE,
    ) as u64
}

fn assert_exact(heap: &GcHeap, expected_cells: u64, expected_reserved: u64) {
    let stats = heap.stats();
    assert_eq!(stats.allocated_bytes as u64, expected_cells);
    assert_eq!(stats.reserved_bytes, expected_reserved);
    assert_eq!(heap.tracked_bytes(), expected_cells + expected_reserved);
}

#[test]
fn minor_copy_and_promotion_refund_only_retired_cells_and_backing() {
    let mut heap = GcHeap::with_max_heap_bytes(1024 * 1024).unwrap();
    heap.set_gc_stress(0, false);
    // SAFETY: heap-owned arena outlives this scope and every handle below.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let first = scope.local(heap.alloc(OpaqueLeaf { payload: 317 }).unwrap());
    let first_alias = scope.local(first.get());
    let old_dead = heap.alloc_old(OpaqueLeaf { payload: 991 }).unwrap();
    heap.alloc(OpaqueLeaf { payload: 997 }).unwrap();
    let dead_token = heap.reserve_external(512).unwrap();
    heap.alloc(Backing {
        _external: dead_token,
        child: RawGc::NULL,
    })
    .unwrap();
    let retained_token = heap.reserve_external(4096).unwrap();
    let before = first.get().offset();
    let cycles = heap.gc_cycle_counts();
    heap.collect_minor(EmptyRoots).unwrap();
    assert_eq!(heap.gc_cycle_counts(), (cycles.0 + 1, cycles.1));
    assert_ne!(first.get().offset(), before);
    assert_eq!(first.get(), first_alias.get());
    let stats = heap.stats().last_scavenge;
    assert_eq!(stats.copied_bytes as u64, bytes::<OpaqueLeaf>(0));
    assert_eq!(stats.promoted_bytes, 0);
    assert_eq!(
        stats.reclaimed_bytes as u64,
        bytes::<OpaqueLeaf>(0) + bytes::<Backing>(0)
    );
    assert_exact(&heap, 2 * bytes::<OpaqueLeaf>(0), 4096);
    // Minor collection does not sweep an unreachable old body.
    heap.read_payload(old_dead, |body| assert_eq!(body.payload, 991));
    let last = scope.local(heap.alloc(OpaqueLeaf { payload: 719 }).unwrap());
    let last_alias = scope.local(last.get());
    let owner = scope.local(
        heap.alloc_old(OpaquePair {
            first: first.get().raw(),
            second: last.get().raw(),
        })
        .unwrap(),
    );
    heap.alloc(OpaqueLeaf { payload: 1009 }).unwrap();
    let before = [first.get().offset(), last.get().offset()];
    heap.collect_minor(EmptyRoots).unwrap();
    let stats = heap.stats().last_scavenge;
    // Explicit handles are visited before remembered parents: the first child
    // has survived once and promotes, while the fresh last child copies once.
    assert_eq!(stats.copied_bytes as u64, bytes::<OpaqueLeaf>(0));
    assert_eq!(stats.promoted_bytes as u64, bytes::<OpaqueLeaf>(0));
    assert_eq!(stats.reclaimed_bytes as u64, bytes::<OpaqueLeaf>(0));
    for (current, previous) in [first.get().offset(), last.get().offset()]
        .into_iter()
        .zip(before)
    {
        assert_ne!(current, previous);
    }
    assert_eq!(first.get(), first_alias.get());
    assert_eq!(last.get(), last_alias.get());
    heap.read_payload(owner.get(), |body| {
        assert_eq!(body.first, first.get().raw());
        assert_eq!(body.second, last.get().raw());
    });
    assert_exact(
        &heap,
        3 * bytes::<OpaqueLeaf>(0) + bytes::<OpaquePair>(0),
        4096,
    );
    let before = [first.get().offset(), last.get().offset()];
    let cycles = heap.gc_cycle_counts();
    heap.collect_minor(EmptyRoots).unwrap();
    assert_eq!(heap.gc_cycle_counts(), (cycles.0 + 1, cycles.1));
    let stats = heap.stats().last_scavenge;
    assert_eq!(stats.copied_bytes, 0);
    assert_eq!(stats.promoted_bytes as u64, bytes::<OpaqueLeaf>(0));
    assert_eq!(stats.reclaimed_bytes, 0);
    assert_eq!(
        first.get().offset(),
        before[0],
        "already-old child is stable"
    );
    assert_ne!(last.get().offset(), before[1], "copied child now promotes");
    assert_eq!(first.get(), first_alias.get());
    assert_eq!(last.get(), last_alias.get());
    heap.read_payload(owner.get(), |body| {
        assert_eq!(body.first, first.get().raw());
        assert_eq!(body.second, last.get().raw());
    });
    heap.read_payload(first.get(), |body| assert_eq!(body.payload, 317));
    heap.read_payload(last.get(), |body| assert_eq!(body.payload, 719));
    assert_exact(
        &heap,
        3 * bytes::<OpaqueLeaf>(0) + bytes::<OpaquePair>(0),
        4096,
    );
    heap.collect_full(&mut |_| {}).unwrap();
    assert_exact(
        &heap,
        2 * bytes::<OpaqueLeaf>(0) + bytes::<OpaquePair>(0),
        4096,
    );
    drop(retained_token);
    heap.drain_external_releases();
    assert_exact(
        &heap,
        2 * bytes::<OpaqueLeaf>(0) + bytes::<OpaquePair>(0),
        0,
    );
}

#[test]
fn every_completed_sweep_publishes_reclaimed_cap_without_another_collection() {
    for mode in ["full", "split", "incremental"] {
        let cap = 1024 * 1024;
        let mut heap = GcHeap::with_max_heap_bytes(cap).unwrap();
        heap.set_gc_stress(0, false);
        // SAFETY: heap-owned arena outlives this scope and every handle below.
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let first = scope.local(heap.alloc(OpaqueLeaf { payload: 317 }).unwrap());
        let last = scope.local(heap.alloc(OpaqueLeaf { payload: 719 }).unwrap());
        let alias = scope.local(first.get());
        let owner = scope.local(
            heap.alloc_old(OpaquePair {
                first: first.get().raw(),
                second: last.get().raw(),
            })
            .unwrap(),
        );
        let live_los = scope.local(
            heap.alloc_variable_with_roots(OpaqueLeaf { payload: 811 }, 160 * 1024, &mut |_| {})
                .unwrap(),
        );
        heap.alloc_variable_with_roots(OpaqueLeaf { payload: 991 }, 512, &mut |_| {})
            .unwrap();
        heap.alloc_variable_with_roots(OpaqueLeaf { payload: 997 }, 160 * 1024, &mut |_| {})
            .unwrap();
        let dead_token = heap.reserve_external(2048).unwrap();
        heap.alloc_old(Backing {
            _external: dead_token,
            child: RawGc::NULL,
        })
        .unwrap();
        let retained_token = heap.reserve_external(4096).unwrap();
        let before = [first.get().offset(), last.get().offset()];
        let cycles = heap.gc_cycle_counts();
        match mode {
            "full" => heap.collect_full(&mut |_| {}).unwrap(),
            "split" => {
                heap.mark_phase(&mut |_| {}).unwrap();
                heap.run_post_mark_processing();
                heap.sweep_phase();
            }
            "incremental" => {
                heap.start_incremental_mark_phase(&mut |_| {}).unwrap();
                let _ = heap.incremental_mark_step(1);
                heap.finish_incremental_mark_phase(&mut |_| {});
                heap.run_post_mark_processing();
                heap.sweep_phase();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            heap.gc_cycle_counts(),
            (cycles.0 + 1, cycles.1 + 1),
            "{mode}"
        );
        for (current, previous) in [first.get().offset(), last.get().offset()]
            .into_iter()
            .zip(before)
        {
            assert_ne!(current, previous, "{mode}: actual nursery relocation");
        }
        assert_eq!(first.get(), alias.get());
        heap.read_payload(owner.get(), |body| {
            assert_eq!(body.first, first.get().raw());
            assert_eq!(body.second, last.get().raw());
        });
        heap.read_payload(first.get(), |body| assert_eq!(body.payload, 317));
        heap.read_payload(last.get(), |body| assert_eq!(body.payload, 719));
        heap.read_payload(live_los.get(), |body| assert_eq!(body.payload, 811));
        let live =
            2 * bytes::<OpaqueLeaf>(0) + bytes::<OpaquePair>(0) + bytes::<OpaqueLeaf>(160 * 1024);
        assert_exact(&heap, live, 4096);
        let cycles = heap.gc_cycle_counts();
        let reserve = cap - heap.tracked_bytes();
        heap.reserve_bytes_no_collect(reserve)
            .expect("explicit sweep exposes exact reclaimed headroom");
        assert_eq!(
            heap.gc_cycle_counts(),
            cycles,
            "{mode}: no emergency settlement"
        );
        assert_exact(&heap, live, 4096 + reserve);
        let error = heap
            .alloc_old(OpaqueLeaf { payload: 1009 })
            .expect_err("real exhausted cap");
        assert!(matches!(error, OutOfMemory::HeapCapExceeded { .. }));
        assert_eq!(error.requested_bytes(), bytes::<OpaqueLeaf>(0));
        assert_eq!(error.heap_limit_bytes(), cap);
        assert_eq!(heap.gc_cycle_counts().1, cycles.1 + 1);
        assert_exact(&heap, live, 4096 + reserve);
        heap.release_bytes(reserve);
        drop(retained_token);
        heap.drain_external_releases();
        assert_exact(&heap, live, 0);
    }
}

#[test]
fn diagnostic_cell_charges_once_without_refunding_a_live_lab_twice() {
    let mut heap = GcHeap::with_max_heap_bytes(1024 * 1024).unwrap();
    heap.set_gc_stress(0, false);
    // SAFETY: heap-owned arena outlives this scope and every handle below.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let child = scope.local(heap.alloc(OpaqueLeaf { payload: 317 }).unwrap());
    let alias = scope.local(child.get());
    let token = heap.reserve_external(4096).unwrap();
    let lab = (heap.lab.top, heap.lab.limit, heap.lab_charged);
    assert!(lab.0 < lab.1 && lab.2 != 0);
    let raw = heap.tracked_bytes;
    let cycles = heap.gc_cycle_counts();
    let diagnostic = scope.local(
        heap.alloc_old_diagnostic_trailing(
            OpaquePair {
                first: child.get().raw(),
                second: child.get().raw(),
            },
            13,
        )
        .unwrap(),
    );
    assert_eq!((heap.lab.top, heap.lab.limit, heap.lab_charged), lab);
    assert_eq!(heap.tracked_bytes, raw + bytes::<OpaquePair>(13));
    assert_eq!(heap.gc_cycle_counts(), cycles);
    assert_eq!(child.get(), alias.get());
    assert_exact(
        &heap,
        bytes::<OpaqueLeaf>(0) + bytes::<OpaquePair>(13),
        4096,
    );
    let before = child.get().offset();
    heap.collect_full(&mut |_| {}).unwrap();
    assert_ne!(child.get().offset(), before);
    assert_eq!(child.get(), alias.get());
    heap.read_payload(diagnostic.get(), |body| {
        assert_eq!(body.first, child.get().raw());
        assert_eq!(body.second, child.get().raw());
    });
    assert_exact(
        &heap,
        bytes::<OpaqueLeaf>(0) + bytes::<OpaquePair>(13),
        4096,
    );
    drop(token);
    heap.drain_external_releases();
    assert_exact(&heap, bytes::<OpaqueLeaf>(0) + bytes::<OpaquePair>(13), 0);
}

#[test]
fn disabled_cap_sweep_releases_backing_without_enabling_cell_accounting() {
    let mut heap = GcHeap::new().unwrap();
    heap.set_gc_stress(0, false);
    assert_eq!(heap.max_heap_bytes(), 0);
    let token = heap.reserve_external(4096).unwrap();
    heap.alloc_old(Backing {
        _external: token,
        child: RawGc::NULL,
    })
    .unwrap();
    assert_eq!(heap.stats().reserved_bytes, 4096);
    heap.collect_full(&mut |_| {}).unwrap();
    assert_eq!(heap.stats().allocated_bytes, 0);
    assert_eq!(heap.stats().reserved_bytes, 0);
    assert_eq!(heap.tracked_bytes, 0);
    assert_eq!(heap.tracked_bytes(), 0);
}

#[test]
fn minor_reclaimed_input_uses_aligned_cells_and_excludes_old_los() {
    let mut heap = GcHeap::with_max_heap_bytes(1024 * 1024).unwrap();
    heap.set_gc_stress(0, false);
    // SAFETY: heap-owned arena outlives this scope and every handle below.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let child = scope.local(
        heap.alloc_trailing_with_roots(OpaqueLeaf { payload: 317 }, 13, &mut |_| {})
            .unwrap(),
    );
    let alias = scope.local(child.get());
    heap.alloc_trailing_with_roots(OpaqueLeaf { payload: 719 }, 9, &mut |_| {})
        .unwrap();
    let los = scope.local(
        heap.alloc_variable_with_roots(OpaqueLeaf { payload: 811 }, 160 * 1024, &mut |_| {})
            .unwrap(),
    );
    let los_offset = los.get().offset();
    let before = child.get().offset();
    heap.collect_minor(EmptyRoots).unwrap();
    let stats = heap.stats().last_scavenge;
    assert_eq!(stats.copied_bytes as u64, bytes::<OpaqueLeaf>(13));
    assert_eq!(stats.promoted_bytes, 0);
    assert_eq!(stats.reclaimed_bytes as u64, bytes::<OpaqueLeaf>(9));
    assert_ne!(child.get().offset(), before);
    assert_eq!(los.get().offset(), los_offset);
    assert_eq!(child.get(), alias.get());
    let live = bytes::<OpaqueLeaf>(13) + bytes::<OpaqueLeaf>(160 * 1024);
    assert_exact(&heap, live, 0);
    let before = child.get().offset();
    heap.collect_minor(EmptyRoots).unwrap();
    let stats = heap.stats().last_scavenge;
    assert_eq!(stats.copied_bytes, 0);
    assert_eq!(stats.promoted_bytes as u64, bytes::<OpaqueLeaf>(13));
    assert_eq!(stats.reclaimed_bytes, 0);
    assert_ne!(child.get().offset(), before);
    assert_eq!(los.get().offset(), los_offset);
    assert_eq!(child.get(), alias.get());
    heap.read_payload(child.get(), |body| assert_eq!(body.payload, 317));
    heap.read_payload(los.get(), |body| assert_eq!(body.payload, 811));
    assert_exact(&heap, live, 0);
}
