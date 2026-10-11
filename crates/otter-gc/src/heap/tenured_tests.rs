//! Site tags, survival decisions and the old-space buffer.
//!
//! # Contents
//! - A scavenge finds each tagged survivor once and decides from survival.
//! - Old placement remembers young children and keeps pages walkable.
//! - Retirement returns the unused tail to the cap.
//!
//! # Invariants
//! Every surviving cell is held through a collector-traced handle.

use super::super::*;
use crate::HandleScope;
use crate::pretenuring::PretenureDecision;
use crate::test_support::{OpaqueLeaf, OpaquePair};

fn heap() -> GcHeap {
    let mut heap = GcHeap::new().expect("heap");
    heap.set_gc_stress(0, false);
    heap
}

fn at_site<T: Traceable>(heap: &mut GcHeap, value: T, placement: SitePlacement) -> Gc<T> {
    heap.alloc_trailing_at_site_with_roots_initialized(value, 0, placement, &mut |_| {}, |_| {})
        .expect("site allocation")
}

#[test]
fn a_scavenge_finds_each_tagged_survivor_once_and_decides_from_survival() {
    let mut heap = heap();
    // SAFETY: the heap-owned handle stack outlives every handle below.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let [kept, dropped] = [(); 2].map(|()| heap.allocation_sites_mut().register());
    let mut survivors = Vec::new();
    for index in 0..150 {
        let cell = at_site(
            &mut heap,
            OpaqueLeaf { payload: index },
            SitePlacement::Young(kept),
        );
        // SAFETY: the cell was just allocated young.
        assert_eq!(unsafe { (*cell.as_header_ptr()).allocation_site() }, kept);
        if index < 140 {
            survivors.push(scope.local(cell));
        }
        let lost = at_site(
            &mut heap,
            OpaqueLeaf { payload: index },
            SitePlacement::Young(dropped),
        );
        if index < 10 {
            survivors.push(scope.local(lost));
        }
    }
    heap.collect_minor_with_roots(&mut |_| {})
        .expect("minor GC");
    let sites = heap.allocation_sites();
    assert!(!heap.new_space.at_maximum());
    assert_eq!(sites.decision(kept), PretenureDecision::MaybeTenure);
    assert_eq!(sites.decision(dropped), PretenureDecision::DontTenure);
    for survivor in &survivors {
        // SAFETY: each survivor is a live traced handle.
        assert_eq!(
            unsafe { (*survivor.get().as_header_ptr()).allocation_site() },
            0
        );
    }
    // A second scavenge finds none of the cleared survivors again.
    heap.allocation_sites_mut()
        .set_decision_for_test(kept, PretenureDecision::Undecided);
    heap.collect_minor_with_roots(&mut |_| {})
        .expect("minor GC");
    assert_eq!(
        heap.allocation_sites().decision(kept),
        PretenureDecision::Undecided
    );
}

#[test]
fn old_placement_remembers_young_children_and_keeps_the_page_walkable() {
    let mut heap = heap();
    // SAFETY: the heap-owned handle stack outlives every handle below.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    // The child is held by no handle: only the old parent's remembered edge
    // keeps it alive through the scavenge below.
    let child = heap.alloc(OpaqueLeaf { payload: 41 }).expect("young child");
    let child_raw = child.raw();
    let parent = at_site(
        &mut heap,
        OpaquePair {
            first: child_raw,
            second: RawGc::NULL,
        },
        SitePlacement::Old,
    );
    let parent = scope.local(parent);
    // SAFETY: both handles are live.
    unsafe {
        assert!(!(*parent.get().as_header_ptr()).is_young());
        assert!((*child.as_header_ptr()).is_young());
    }
    assert!(heap.remembered_parents.contains(&parent.get().raw()));
    assert!(
        heap.old_lab.top < heap.old_lab.limit,
        "the buffer stays live"
    );
    let second = scope.local(at_site(
        &mut heap,
        OpaqueLeaf { payload: 43 },
        SitePlacement::Old,
    ));
    assert_eq!(
        second.get().raw().0 as usize,
        parent.get().raw().0 as usize
            + align_up(
                std::mem::size_of::<GcHeader>() + std::mem::size_of::<OpaquePair>(),
                CELL_SIZE
            ),
        "consecutive tenured cells bump one buffer"
    );
    // A page walk sees both cells and the filler-capped tail.
    let mut seen = Vec::new();
    // SAFETY: no allocation happens during the walk.
    unsafe {
        heap.for_each_live_object(|header| {
            if !(*header).is_young() && (*header).type_tag() == OpaqueLeaf::TYPE_TAG {
                seen.push(header as usize);
            }
        });
    }
    assert!(seen.contains(&(second.get().as_header_ptr() as usize)));
    // The child survives through its remembered old parent only.
    heap.collect_minor_with_roots(&mut |_| {})
        .expect("minor GC");
    let moved = heap.read_payload(parent.get(), |body| body.first);
    assert_ne!(moved, child_raw);
    // SAFETY: the moved child is live through its parent.
    let leaf = unsafe { Gc::<OpaqueLeaf>::from_offset(moved.0) };
    heap.read_payload(leaf, |body| assert_eq!(body.payload, 41));
    assert_eq!(heap.old_lab.limit, 0, "a collection retires the buffer");
    heap.collect_full(&mut |_| {}).expect("full GC");
    heap.read_payload(second.get(), |body| assert_eq!(body.payload, 43));
}

#[test]
fn retirement_returns_the_unused_tail_to_the_cap() {
    let mut heap = GcHeap::with_max_heap_bytes(8 * 1024 * 1024).expect("heap");
    heap.set_gc_stress(0, false);
    // SAFETY: the heap-owned handle stack outlives every handle below.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    heap.retire_machine_labs();
    let before = heap.tracked_bytes;
    let cell = scope.local(at_site(
        &mut heap,
        OpaqueLeaf { payload: 7 },
        SitePlacement::Old,
    ));
    let bytes = align_up(
        std::mem::size_of::<GcHeader>() + std::mem::size_of::<OpaqueLeaf>(),
        CELL_SIZE,
    ) as u64;
    assert_eq!(heap.tracked_bytes, before + OLD_LAB_BYTES as u64);
    assert_eq!(heap.effective_tracked_bytes(), before + bytes);
    heap.retire_machine_labs();
    assert_eq!(heap.tracked_bytes, before + bytes);
    heap.read_payload(cell.get(), |body| assert_eq!(body.payload, 7));
}
