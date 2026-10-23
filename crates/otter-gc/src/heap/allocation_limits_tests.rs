//! Real heap pressure for the one-shot startup allocation budget.
//!
//! # Contents
//! - A rooted young chain grows to 64 MiB with major-collection headroom.
//! - Promoted cohorts die before the first natural major, which reclaims them.
//!
//! # Invariants
//! - Every retained edge is held by the heap's traced embedder slots.
//! - Neither test forces a full collection or changes the default old limit.
//! - Promotion fixtures use real minor evacuation and physical cell bytes.
//!
//! # See also
//! - [`super::GcHeap::seed_initial_old_generation_limit`] owns the one-shot update.

use super::*;
use crate::compressed::RawGc;
use crate::header::GcHeader;
use crate::test_support::OpaqueLeaf;
use crate::trace::{SlotVisitor, Traceable};

const MIB: u64 = 1024 * 1024;
const CELL_BYTES: usize = 8 * 1024;

struct ChainCell {
    next: RawGc,
    marker: u64,
}

impl Traceable for ChainCell {
    const TYPE_TAG: u8 = 0xec;

    unsafe fn trace_slots(this: *mut Self, visitor: &mut SlotVisitor<'_>) {
        // SAFETY: the collector supplies an initialized ChainCell body.
        unsafe { visitor(std::ptr::addr_of_mut!((*this).next)) };
    }
}

fn heap() -> GcHeap {
    let mut heap = GcHeap::new().expect("heap");
    heap.set_gc_stress(0, false);
    heap
}

fn append(heap: &mut GcHeap, root: usize, marker: u64) {
    let cell = heap
        .alloc_trailing_with_roots(
            ChainCell {
                next: heap.embedder_root(root),
                marker,
            },
            CELL_BYTES - std::mem::size_of::<GcHeader>() - std::mem::size_of::<ChainCell>(),
            &mut |_| {},
        )
        .expect("young chain cell");
    // There is no allocation between the returned cell and its traced slot.
    heap.set_embedder_root(root, cell.raw());
}

#[test]
fn bootstrap_live_young_chain_keeps_headroom_through_sixty_four_mib() {
    let mut heap = heap();
    let cells = 64 * MIB / CELL_BYTES as u64;
    let mut seeded_limit = None;
    let mut previous_major = 0;
    let mut previous_major_allocation = None;
    for index in 0..cells {
        append(&mut heap, 0, index);
        if seeded_limit.is_none() && !heap.initial_old_limit_pending {
            seeded_limit = Some(heap.old_generation_limit);
        }
        let majors = heap.gc_stats().gc_cycles;
        if majors != previous_major {
            if let Some(previous) = previous_major_allocation {
                assert!(
                    index > previous + 1,
                    "live growth cannot collect on every allocation"
                );
            }
            assert!(
                heap.old_generation_limit
                    >= heap.old_generation_consumed_bytes() + MINIMUM_ALLOCATION_LIMIT_GROWING_STEP,
                "each successful major grants actual allocation headroom"
            );
            previous_major_allocation = Some(index);
            previous_major = majors;
        }
    }
    assert!(
        heap.gc_stats().minor_gc_cycles > 0,
        "young traffic must seed the limit"
    );
    assert!(seeded_limit.unwrap() < INITIAL_OLD_GENERATION_SIZE);
    assert!(
        (1..8).contains(&heap.gc_stats().gc_cycles),
        "bounded major count for fully live growth"
    );
    let mut raw = heap.embedder_root(0);
    for expected in (0..cells).rev() {
        // SAFETY: the sole chain root and each traced link contain ChainCell handles.
        let cell = unsafe { raw.cast::<ChainCell>() };
        raw = heap.read_payload(cell, |body| {
            assert_eq!(body.marker, expected);
            body.next
        });
    }
    assert!(raw.is_null());
}

#[test]
fn bootstrap_natural_major_reclaims_promoted_garbage_before_ninety_six_mib() {
    let mut heap = heap();
    let sentinel = heap.alloc(OpaqueLeaf { payload: 317 }).unwrap();
    heap.set_embedder_root(0, sentinel.raw());
    let mut peak_old = 0;
    'cohorts: for cohort in 0..48 {
        for index in 0..(2 * MIB / CELL_BYTES as u64) {
            append(&mut heap, 1, cohort * 1000 + index);
            peak_old = peak_old.max(heap.old_generation_consumed_bytes());
            if heap.gc_stats().gc_cycles != 0 {
                break 'cohorts;
            }
        }
        // Only promotion is explicit: two real scavenges keep this whole
        // young cohort, then its root is dropped before further traffic.
        heap.collect_minor(crate::EmptyRoots).unwrap();
        heap.collect_minor(crate::EmptyRoots).unwrap();
        peak_old = peak_old.max(heap.old_generation_consumed_bytes());
        heap.set_embedder_root(1, RawGc::NULL);
    }
    assert_eq!(
        heap.gc_stats().gc_cycles,
        1,
        "allocation pressure must run the first major"
    );
    assert!(!heap.initial_old_limit_pending);
    assert!(
        peak_old < 96 * MIB,
        "startup cannot retain the old 256 MiB allowance"
    );
    assert!(heap.stats().last_full_reclaimed as u64 >= 8 * MIB);
    // SAFETY: root zero retains the same OpaqueLeaf through every collection.
    let sentinel = unsafe { heap.embedder_root(0).cast::<OpaqueLeaf>() };
    heap.read_payload(sentinel, |body| assert_eq!(body.payload, 317));
}

#[test]
fn bootstrap_explicit_override_survives_first_minor_feedback() {
    let mut heap = heap();
    let override_limit = 64 * MIB;
    heap.set_initial_old_generation_size(override_limit);
    let threshold = heap.concurrent_marking_start;
    let sentinel = heap.alloc(OpaqueLeaf { payload: 719 }).unwrap();
    heap.set_embedder_root(0, sentinel.raw());
    let before = heap.embedder_root(0);
    let cycles = heap.gc_cycle_counts();
    heap.collect_minor(crate::EmptyRoots).unwrap();
    assert_eq!(heap.gc_cycle_counts(), (cycles.0 + 1, cycles.1));
    assert_ne!(
        heap.embedder_root(0),
        before,
        "the rooted body really moved"
    );
    assert!(!heap.initial_old_limit_pending);
    assert_eq!(heap.old_generation_limit, override_limit);
    assert_eq!(heap.concurrent_marking_start, threshold);
    // SAFETY: root zero keeps the OpaqueLeaf through the actual scavenge.
    let sentinel = unsafe { heap.embedder_root(0).cast::<OpaqueLeaf>() };
    heap.read_payload(sentinel, |body| assert_eq!(body.payload, 719));
}
