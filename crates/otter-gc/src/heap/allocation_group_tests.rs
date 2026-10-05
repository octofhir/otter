//! Exact LAB admission accounting and first-source recovery after group refusal.
//!
//! # Contents
//! - Existing-fit admission cannot charge, bump or fabricate a cell.
//! - Completed cap collection refuses only the group while preserving a legal prefix.
//!
//! # Invariants
//! The tests use the one real heap owner, collector and canonical allocator.
//! They never replay members or initialize unpublished cells during admission.
//! Roots are registered stationary engine locals until every collection ends.
//!
//! # See also
//! - `super::GcHeap::ensure_machine_allocation_with_roots`.

use super::*;
use crate::test_support::OpaqueLeaf;

#[test]
fn ensure_charges_one_window_and_repeated_fit_never_advances_or_accounts() {
    let mut heap = GcHeap::with_max_heap_bytes(256).unwrap();
    heap.set_gc_stress(0, false);
    let before = heap.gc_stats().clone();
    assert!(
        heap.ensure_machine_allocation_with_roots(128, &mut |_| {})
            .unwrap()
    );
    let lab = heap.lab;
    assert_eq!(lab.remaining(), 256);
    assert_eq!(heap.tracked_bytes, 256);
    assert_eq!(heap.gc_stats().alloc_bytes_total, before.alloc_bytes_total);
    assert!(
        heap.ensure_machine_allocation_with_roots(128, &mut |_| {})
            .unwrap()
    );
    assert_eq!(heap.lab, lab);
    assert_eq!(
        heap.tracked_bytes, 256,
        "a fit never charges already-admitted bytes twice"
    );
    let first = heap.alloc(OpaqueLeaf { payload: 73 }).unwrap();
    let cell_bytes = align_up(
        std::mem::size_of::<GcHeader>() + std::mem::size_of::<OpaqueLeaf>(),
        CELL_SIZE,
    );
    assert_eq!(first.offset(), lab.top as u32);
    assert_eq!(heap.tracked_bytes, 256);
    assert_eq!(
        heap.gc_stats().alloc_bytes_total,
        before.alloc_bytes_total + cell_bytes as u64
    );
    heap.retire_lab();
    assert_eq!(
        heap.tracked_bytes, cell_bytes as u64,
        "retirement refunds exactly the unused tail"
    );
    assert_eq!(heap.stats().allocated_bytes, cell_bytes);
    assert!(
        !heap
            .ensure_machine_allocation_with_roots(0, &mut |_| {})
            .unwrap()
    );
    assert!(
        !heap
            .ensure_machine_allocation_with_roots(CELL_SIZE + 1, &mut |_| {})
            .unwrap()
    );
    assert!(
        !heap
            .ensure_machine_allocation_with_roots(
                crate::page::PAGE_PAYLOAD_SIZE + CELL_SIZE,
                &mut |_| {}
            )
            .unwrap()
    );
    assert_eq!(heap.tracked_bytes, cell_bytes as u64);
}

#[test]
fn cap_group_refusal_rewrites_real_roots_without_latching_or_suppressing_the_prefix() {
    let mut heap = GcHeap::with_max_heap_bytes(256).unwrap();
    heap.set_gc_stress(0, false);
    let mut survivor = heap.alloc(OpaqueLeaf { payload: 731 }).unwrap();
    let mut scope = crate::RootScope::new(&mut heap);
    // SAFETY: the typed root is stationary until this registered scope drops.
    unsafe {
        scope.add_raw_slot((&mut survivor as *mut Gc<OpaqueLeaf>).cast());
    }
    heap.retire_lab();
    let cell_bytes = align_up(
        std::mem::size_of::<GcHeader>() + std::mem::size_of::<OpaqueLeaf>(),
        CELL_SIZE,
    );
    assert_eq!(cell_bytes, 16);
    let pressure = 256 - 2 * cell_bytes as u64;
    heap.reserve_bytes_with_roots(pressure, &mut |_| {})
        .unwrap();
    assert_eq!(heap.tracked_bytes, 256 - cell_bytes as u64);
    let old = survivor;
    let before = heap.gc_stats().clone();
    heap.start_gc_pause_capture(4).unwrap();
    assert!(
        !heap
            .ensure_machine_allocation_with_roots(2 * cell_bytes, &mut |_| {})
            .unwrap()
    );
    let capture = heap.take_gc_pause_capture().unwrap();
    assert_eq!(capture.records.len(), 1);
    assert_eq!(capture.records[0].kind, GcPauseKind::Full);
    assert_eq!(capture.records[0].trigger, GcPauseTrigger::HeapCap);
    assert_eq!(
        capture.records[0].outcome,
        GcPauseOutcome::CompletedAllocationRefused
    );
    assert!(!capture.incomplete);
    assert_eq!(capture.dropped_records, 0);
    assert_ne!(
        survivor, old,
        "successful emergency collection rewrites the canonical root"
    );
    assert_eq!(heap.read_payload(survivor, |body| body.payload), 731);
    assert!(heap.gc_stats().minor_gc_cycles > before.minor_gc_cycles);
    assert_eq!(heap.gc_stats().alloc_bytes_total, before.alloc_bytes_total);
    assert_eq!(heap.reserved_bytes, pressure);
    assert_eq!(heap.tracked_bytes, pressure + cell_bytes as u64);
    assert_eq!(heap.lab, LinearAllocationArea::EMPTY);
    assert!(!heap.oom_flag.load(Ordering::Relaxed));
    let first = heap
        .alloc(OpaqueLeaf { payload: 913 })
        .expect("the legal first source cell fits");
    assert_eq!(heap.read_payload(first, |body| body.payload), 913);
    assert_eq!(
        heap.gc_stats().alloc_bytes_total,
        before.alloc_bytes_total + cell_bytes as u64
    );
    assert_eq!(heap.tracked_bytes, 256);
    assert_eq!(heap.read_payload(survivor, |body| body.payload), 731);
    heap.release_bytes(pressure);
}

#[test]
fn current_policy_changes_refuse_ensure_without_top_or_cell_publication() {
    let mut heap = GcHeap::new().unwrap();
    heap.set_gc_stress(0, false);
    assert!(heap.machine_allocation_allowed());
    assert!(
        heap.ensure_machine_allocation_with_roots(32, &mut |_| {})
            .unwrap()
    );
    heap.set_gc_stress(1, false);
    assert!(!heap.machine_allocation_allowed());
    let before = heap.gc_stats().clone();
    let lab = heap.lab;
    assert!(
        !heap
            .ensure_machine_allocation_with_roots(32, &mut |_| {})
            .unwrap()
    );
    assert_eq!(heap.lab, lab);
    assert_eq!(heap.gc_stats().alloc_bytes_total, before.alloc_bytes_total);
    assert_eq!(heap.gc_stats().minor_gc_cycles, before.minor_gc_cycles);
    assert!(!heap.oom_flag.load(Ordering::Relaxed));
    heap.set_gc_stress(0, false);
    heap.set_tenure_all(true);
    assert!(!heap.machine_allocation_allowed());
    assert!(
        !heap
            .ensure_machine_allocation_with_roots(32, &mut |_| {})
            .unwrap()
    );
    heap.set_tenure_all(false);
    assert!(heap.machine_allocation_allowed());
    heap.start_incremental_mark_phase(&mut |_| {}).unwrap();
    assert!(!heap.machine_allocation_allowed());
    let before = heap.gc_stats().clone();
    let lab = heap.lab;
    assert!(
        !heap
            .ensure_machine_allocation_with_roots(32, &mut |_| {})
            .unwrap()
    );
    assert_eq!(heap.lab, lab);
    assert_eq!(heap.gc_stats().alloc_bytes_total, before.alloc_bytes_total);
    assert!(!heap.oom_flag.load(Ordering::Relaxed));
    heap.finish_incremental_mark_phase(&mut |_| {});
    assert!(
        !heap.machine_allocation_allowed(),
        "finished marking remains active until canonical sweep"
    );
    heap.sweep_phase();
    assert!(heap.machine_allocation_allowed());
}
