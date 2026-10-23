//! Full-sweep-only physical nursery release with real moving survivors.
//!
//! # Contents
//! - Minor and Throughput collections retain payload; ReleaseMemory discards it.
//! - Capture enabled/disabled preserves the same physical and semantic result.
//! - Live old/young roots, one-shot drop hooks, capacity and writable reuse.
//!
//! # Invariants
//! - Raw canaries lie strictly outside every published cell, never in headers.
//! - Live handles are reloaded from embedder roots after moving collection.
//! - No test changes nursery capacity or the old-generation budget.
//!
//! # See also
//! - [`super::GcHeap::sweep_phase_with_pause_start`] owns the discard boundary.
//! - [`crate::compressed::discard`] verifies actual OS rounding and access.

use super::*;
use crate::FullGcGoal;
use crate::page::PAGE_SIZE;
use crate::test_support::OpaqueLeaf;
use crate::trace::{SlotVisitor, Traceable};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct DropProbe {
    dropped: Arc<AtomicUsize>,
    marker: u64,
}

impl Traceable for DropProbe {
    const TYPE_TAG: u8 = 0xe9;
    unsafe fn trace_slots(_this: *mut Self, _visitor: &mut SlotVisitor<'_>) {}
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        assert_eq!(self.marker, 0x7149, "drop reads its initialized corpse");
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

fn nursery_pages(heap: &GcHeap) -> Vec<u32> {
    let mut pages: Vec<_> = heap
        .new_space
        .from_pages()
        .iter()
        .chain(heap.new_space.to_pages())
        .map(|page| page.cage_offset())
        .collect();
    pages.sort_unstable();
    pages
}

#[test]
fn nursery_discard_waits_for_full_sweep_and_preserves_live_survivors_and_capacity() {
    for capturing in [false, true] {
        let mut heap = GcHeap::new().unwrap();
        heap.set_gc_stress(0, false);
        if capturing {
            heap.start_gc_pause_capture(8).unwrap();
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let _ = heap
            .alloc(DropProbe {
                dropped: dropped.clone(),
                marker: 0x7149,
            })
            .unwrap();
        let keep = heap.alloc(OpaqueLeaf { payload: 317 }).unwrap();
        heap.set_embedder_root(0, keep.raw());
        heap.retire_machine_labs();
        let page = &heap.new_space.from_pages()[0];
        assert!(page.header().bump_cursor < PAGE_SIZE - 1);
        // SAFETY: the last byte is beyond the published allocation cursor. Its
        // cage page remains owned by the nursery through minor and full collection.
        let canary = unsafe { page.base_ptr().add(PAGE_SIZE - 1) };
        unsafe { canary.write(0xa5) };
        let pages = nursery_pages(&heap);
        let capacity = heap.new_space.capacity_bytes();
        heap.collect_minor(crate::EmptyRoots).unwrap();
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        // SAFETY: inactive payload stays mapped. A minor must not discard it.
        assert_eq!(unsafe { canary.read() }, 0xa5);

        heap.collect_full_observed(
            &mut |_| {},
            GcPauseTrigger::GrowthBudget,
            FullGcGoal::Throughput,
        )
        .unwrap();
        assert_eq!(heap.new_space.capacity_bytes(), capacity);
        assert_eq!(nursery_pages(&heap), pages);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        // SAFETY: a Throughput full sweep retains the unused physical payload.
        assert_eq!(unsafe { canary.read() }, 0xa5);
        // SAFETY: root zero retained the older leaf through both collections.
        let older = unsafe { heap.embedder_root(0).cast::<OpaqueLeaf>() };
        heap.read_payload(older, |body| assert_eq!(body.payload, 317));

        // This new object is young at full-GC entry and must remain a copied
        // nursery survivor while the older root may be promoted.
        let fresh = heap.alloc(OpaqueLeaf { payload: 719 }).unwrap();
        heap.set_embedder_root(1, fresh.raw());
        heap.retire_machine_labs();
        let unused_tails: Vec<_> = heap
            .new_space
            .from_pages()
            .iter()
            .chain(heap.new_space.to_pages())
            .map(|page| {
                assert!(page.header().bump_cursor < PAGE_SIZE - 1);
                // SAFETY: this byte is beyond the published cursor of every
                // owned page, including the active page holding the fresh leaf.
                let tail = unsafe { page.base_ptr().add(PAGE_SIZE - 1) };
                unsafe { tail.write(0xa5) };
                tail
            })
            .collect();
        heap.collect_full_observed(
            &mut |_| {},
            GcPauseTrigger::Explicit,
            FullGcGoal::ReleaseMemory,
        )
        .unwrap();
        assert_eq!(heap.new_space.capacity_bytes(), capacity);
        assert_eq!(nursery_pages(&heap), pages, "all virtual pages stay owned");
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        for tail in unused_tails {
            #[cfg(unix)]
            // SAFETY: every page remains owned. Both active unused tails and
            // inactive payload were remapped without changing their addresses.
            assert_eq!(unsafe { tail.read() }, 0);
            #[cfg(not(unix))]
            assert_eq!(unsafe { tail.read() }, 0xa5);
        }
        for (root, expected) in [(0, 317), (1, 719)] {
            // SAFETY: the corresponding embedder slot retained this exact leaf.
            let leaf = unsafe { heap.embedder_root(root).cast::<OpaqueLeaf>() };
            heap.read_payload(leaf, |body| assert_eq!(body.payload, expected));
        }
        // SAFETY: root one holds the fresh live leaf's current header.
        let young =
            unsafe { &*(cage_base().add(heap.embedder_root(1).0 as usize) as *const GcHeader) };
        assert!(
            young.is_young(),
            "discard must preserve active young payload"
        );

        const EXTRA: usize = 64 * 1024;
        let reused = heap
            .alloc_trailing_with_roots_initialized(
                OpaqueLeaf { payload: 1103 },
                EXTRA,
                &mut |_| {},
                |body| {
                    // SAFETY: this allocation owns exactly EXTRA trailing initialized
                    // bytes, crossing the discarded OS pages on supported hosts.
                    unsafe {
                        core::ptr::write_bytes(
                            (body as *mut OpaqueLeaf)
                                .cast::<u8>()
                                .add(std::mem::size_of::<OpaqueLeaf>()),
                            0x5a,
                            EXTRA,
                        )
                    };
                },
            )
            .unwrap();
        heap.read_payload(reused, |body| {
            assert_eq!(body.payload, 1103);
            // SAFETY: the allocation above includes this whole trailing range.
            let tail = unsafe {
                (body as *const OpaqueLeaf)
                    .cast::<u8>()
                    .add(std::mem::size_of::<OpaqueLeaf>())
            };
            unsafe {
                assert_eq!(*tail, 0x5a);
                assert_eq!(*tail.add(EXTRA - 1), 0x5a);
            }
        });
        if capturing {
            let capture = heap.take_gc_pause_capture().unwrap();
            assert!(!capture.incomplete);
            assert_eq!(capture.dropped_records, 0);
            let full: Vec<_> = capture
                .records
                .iter()
                .filter(|record| record.kind == GcPauseKind::Full)
                .collect();
            assert_eq!(full.len(), 2);
            assert_eq!(full[0].trigger, GcPauseTrigger::GrowthBudget);
            assert_eq!(full[1].trigger, GcPauseTrigger::Explicit);
            for (index, record) in full.iter().enumerate() {
                assert_eq!(
                    record.outcome,
                    crate::observation::GcPauseOutcome::Completed
                );
                assert_eq!(record.full_cycles_after, index as u64 + 1);
            }
        }
    }
}
