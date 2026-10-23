//! Actual old survivors, reclaimed holes and repeated full-sweep reuse.
//!
//! # Contents
//! - Large dead bodies separated by a rooted graph in one ordinary old page.
//! - Throughput preserves payload; ReleaseMemory preserves headers/links and drops pages.
//! - Capture enabled/disabled has the same drop, walk, reuse and live-value result.
//!
//! # Invariants
//! - The fixture uses real old allocations and ordinary full collection.
//! - All retained bodies are owned by the existing two embedder root slots.
//! - No synthetic free range, page accounting, policy or cache is installed.
//!
//! # See also
//! - [`crate::space::OldSpace`] reuses the sweep's existing recovered ranges.
//! - [`crate::compressed::discard`] verifies actual OS access and rounding.

use super::*;
use crate::FullGcGoal;
use crate::header::{FREE_TAG, HEADER_SIZE};
use crate::page::{PAGE_SIZE, Page};
use crate::test_support::{OpaqueLeaf, OpaquePair};
use crate::trace::{SlotVisitor, Traceable};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const MARKER: u64 = 0x7149_8326_5a5a_a5a5;
const DEAD_BYTES: usize = 64 * 1024;

struct DeadCell {
    dropped: Arc<AtomicUsize>,
    words: [u64; 8190],
}

impl Traceable for DeadCell {
    const TYPE_TAG: u8 = 0xe8;
    unsafe fn trace_slots(_this: *mut Self, _visitor: &mut SlotVisitor<'_>) {}
}

impl Drop for DeadCell {
    fn drop(&mut self) {
        assert_eq!(self.words[0], MARKER);
        assert_eq!(
            self.words[self.words.len() - 1],
            MARKER,
            "the payload reader must finish before any physical discard"
        );
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

fn dead(heap: &mut GcHeap, dropped: &Arc<AtomicUsize>) -> u32 {
    assert_eq!(HEADER_SIZE + size_of::<DeadCell>(), DEAD_BYTES);
    heap.alloc_old(DeadCell {
        dropped: dropped.clone(),
        words: [MARKER; 8190],
    })
    .unwrap()
    .offset()
}

fn walked(page: &Page) -> Vec<(u32, u8, usize)> {
    let mut entries = Vec::new();
    let mut end = PAGE_HEADER_SIZE;
    // SAFETY: this is a retained page after a real sweep. Requiring contiguous
    // coverage through its cursor detects any invalid/zeroed filler header.
    unsafe {
        page.for_each_object(|header, offset| {
            assert_eq!(offset, end);
            let bytes = (*header).size_bytes() as usize;
            assert_ne!(bytes, 0);
            entries.push((
                page.cage_offset() + offset as u32,
                (*header).type_tag(),
                bytes,
            ));
            end += align_up(bytes, CELL_SIZE);
        });
    }
    assert_eq!(
        end,
        page.header().bump_cursor,
        "the entire bump interval stays walkable"
    );
    entries
}

#[test]
fn old_space_discard_preserves_hooks_headers_links_repeated_sweeps_and_exact_reuse() {
    for capturing in [false, true] {
        let mut heap = GcHeap::new().unwrap();
        heap.set_gc_stress(0, false);
        if capturing {
            heap.start_gc_pause_capture(8).unwrap();
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let root = heap
            .alloc_old(OpaquePair {
                first: RawGc::NULL,
                second: RawGc::NULL,
            })
            .unwrap();
        heap.set_embedder_root(0, root.raw());
        let a = dead(&mut heap, &dropped);
        let middle = heap.alloc_old(OpaqueLeaf { payload: 317 }).unwrap();
        heap.with_payload(root, |body| body.first = middle.raw());
        heap.write_barrier(root, middle);
        let b = dead(&mut heap, &dropped);
        let last = heap.alloc_old(OpaqueLeaf { payload: 719 }).unwrap();
        heap.with_payload(root, |body| body.second = last.raw());
        heap.write_barrier(root, last);
        let _tail = dead(&mut heap, &dropped);
        heap.retire_machine_labs();
        assert_eq!(heap.old_space.page_count(), 1);
        let page_offset = heap.old_space.pages()[0].cage_offset();
        // SAFETY: the last page byte is beyond every allocated cell. The page is
        // retained by the rooted graph and remains owned through both collections.
        let tail_canary = unsafe { heap.old_space.pages()[0].base_ptr().add(PAGE_SIZE - 1) };
        unsafe { tail_canary.write(0xa5) };

        for (cycle, goal) in [FullGcGoal::Throughput, FullGcGoal::ReleaseMemory]
            .into_iter()
            .enumerate()
        {
            let trigger = if goal == FullGcGoal::Throughput {
                GcPauseTrigger::GrowthBudget
            } else {
                GcPauseTrigger::Explicit
            };
            heap.collect_full_observed(&mut |_| {}, trigger, goal)
                .unwrap();
            assert_eq!(
                dropped.load(Ordering::Relaxed),
                3,
                "every dead body drops once"
            );
            assert_eq!(heap.old_space.page_count(), 1);
            let page = &heap.old_space.pages()[0];
            assert_eq!(page.cage_offset(), page_offset);
            let entries = walked(page);
            let fillers: Vec<_> = entries.iter().filter(|entry| entry.1 == FREE_TAG).collect();
            assert_eq!(fillers.len(), 2);
            assert_eq!((fillers[0].0, fillers[0].2), (a, DEAD_BYTES));
            assert_eq!((fillers[1].0, fillers[1].2), (b, DEAD_BYTES));
            // SAFETY: these are the two live same-class FREE entries published by
            // the production sweep. The later one links to the first, then zero.
            unsafe {
                let link = |offset| {
                    cage_base()
                        .add(offset as usize + HEADER_SIZE)
                        .cast::<u32>()
                        .read()
                };
                assert_eq!(link(b), a, "discard must retain the intrusive next link");
                assert_eq!(link(a), 0);
            }
            if let Some(granule) = crate::compressed::discard::system_page_size() {
                let start = align_up(a as usize + HEADER_SIZE + size_of::<u32>(), granule);
                let end = (a as usize + DEAD_BYTES) & !(granule - 1);
                if start < end {
                    // SAFETY: these OS pages lie strictly inside the dead run.
                    unsafe {
                        if goal == FullGcGoal::ReleaseMemory {
                            assert_eq!(*cage_base().add(start), 0);
                            assert_eq!(*cage_base().add(end - 1), 0);
                        } else {
                            assert_eq!(cage_base().add(start).cast::<u64>().read(), MARKER);
                            assert_eq!(
                                cage_base().add(end - size_of::<u64>()).cast::<u64>().read(),
                                MARKER
                            );
                        }
                    }
                }
                // SAFETY: the same unused bump-tail address stays mapped.
                assert_eq!(
                    unsafe { tail_canary.read() },
                    if goal == FullGcGoal::ReleaseMemory {
                        0
                    } else {
                        0xa5
                    }
                );
            } else {
                assert_eq!(unsafe { tail_canary.read() }, 0xa5);
            }
            assert_eq!(heap.gc_stats().gc_cycles, cycle as u64 + 1);
            heap.read_payload(middle, |body| assert_eq!(body.payload, 317));
            heap.read_payload(last, |body| assert_eq!(body.payload, 719));
        }

        let extra = DEAD_BYTES - HEADER_SIZE - size_of::<OpaqueLeaf>();
        let reused_b = heap
            .alloc_old_trailing_with_roots(OpaqueLeaf { payload: 1103 }, extra, &mut |_| {})
            .unwrap();
        let reused_a = heap
            .alloc_old_trailing_with_roots(OpaqueLeaf { payload: 1459 }, extra, &mut |_| {})
            .unwrap();
        assert_eq!(reused_b.offset(), b);
        assert_eq!(reused_a.offset(), a);
        assert_eq!(
            heap.old_space.page_count(),
            1,
            "reuse cannot consume another page"
        );
        let second_root = heap
            .alloc_old(OpaquePair {
                first: reused_b.raw(),
                second: reused_a.raw(),
            })
            .unwrap();
        heap.set_embedder_root(1, second_root.raw());
        heap.collect_full(&mut |_| {}).unwrap();
        assert_eq!(dropped.load(Ordering::Relaxed), 3);
        assert_eq!(heap.old_space.page_count(), 1);
        walked(&heap.old_space.pages()[0]);
        for (leaf, expected) in [
            (middle, 317),
            (last, 719),
            (reused_b, 1103),
            (reused_a, 1459),
        ] {
            heap.read_payload(leaf, |body| assert_eq!(body.payload, expected));
        }
        if capturing {
            let capture = heap.take_gc_pause_capture().unwrap();
            assert!(!capture.incomplete);
            assert_eq!(capture.dropped_records, 0);
            let full: Vec<_> = capture
                .records
                .iter()
                .filter(|record| record.kind == GcPauseKind::Full)
                .collect();
            assert_eq!(full.len(), 3);
            assert_eq!(full[0].trigger, GcPauseTrigger::GrowthBudget);
            assert_eq!(full[1].trigger, GcPauseTrigger::Explicit);
            assert_eq!(full[2].trigger, GcPauseTrigger::Explicit);
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
