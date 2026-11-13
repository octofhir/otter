//! Fault injection for remembered-edge verification and shared-slot coverage.
//!
//! # Contents
//! - A live incoming owner makes a deliberately unbarriered edge fatal.
//! - Recording the barrier repairs the edge before actual nursery movement.
//! - Unreferenced garbage is filtered before its stale child is dereferenced.
//! - Remembered and young owners cover slots in another old storage cell.
//!
//! # Invariants
//! - Faults are injected without a safepoint, into test-owned old bodies.
//! - Explicit scopes root every value used across a real collection.
//! - Direct verifier calls avoid process-wide environment changes.
//!
//! # See also
//! - [`super::GcHeap::verify_remembered_complete`] owns the checked contract.

use super::*;
use crate::HandleScope;
use crate::test_support::{OpaqueLeaf, OpaquePair};

fn heap() -> GcHeap {
    let mut heap = GcHeap::new().expect("heap");
    heap.set_gc_stress(0, false);
    heap
}

fn empty_pair() -> OpaquePair {
    OpaquePair {
        first: RawGc(0),
        second: RawGc(0),
    }
}

unsafe fn inject_first(pair: Gc<OpaquePair>, child: RawGc) {
    // SAFETY: this test-owned old pair is live, no collector is active, and
    // no allocation occurs before verification. Omitting the barrier is the
    // intentional fault this test must detect.
    unsafe {
        let body = pair
            .as_header_ptr()
            .cast::<u8>()
            .add(std::mem::size_of::<GcHeader>())
            .cast::<OpaquePair>();
        (*body).first = child;
    }
}

#[test]
fn a_heap_referenced_missing_barrier_is_fatal_and_can_be_repaired_before_gc() {
    let mut heap = heap();
    // SAFETY: the heap and its handle stack outlive all locals in this scope.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let parent = scope.local(heap.alloc_old(empty_pair()).unwrap());
    let owner = scope.local(
        heap.alloc_old(OpaquePair {
            first: parent.get().raw(),
            second: RawGc(0),
        })
        .unwrap(),
    );
    let child = scope.local(heap.alloc(OpaqueLeaf { payload: 719 }).unwrap());
    // SAFETY: all three cells are live and no collection intervenes.
    unsafe {
        inject_first(parent.get(), child.get().raw());
        assert!(!(*parent.get().as_header_ptr()).is_remembered());
        assert!((*child.get().as_header_ptr()).is_young());
    }
    let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        heap.verify_remembered_complete();
    }));
    let message = rejected.expect_err("a verifier miss must stop before nursery movement");
    let text = message
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| message.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        text.contains("GC remembered-set verification failed"),
        "{text}"
    );
    heap.record_write_edge(parent.get(), child.get().raw());
    heap.verify_remembered_complete();
    let before = child.get().offset();
    let cycles = heap.gc_cycle_counts();
    heap.collect_minor_with_roots(&mut |_| {})
        .expect("real nursery movement");
    assert_ne!(child.get().offset(), before);
    assert_eq!(heap.gc_cycle_counts().0, cycles.0 + 1);
    heap.read_payload(owner.get(), |pair| {
        assert_eq!(pair.first, parent.get().raw())
    });
    heap.read_payload(parent.get(), |pair| {
        assert_eq!(pair.first, child.get().raw())
    });
    heap.read_payload(child.get(), |leaf| assert_eq!(leaf.payload, 719));
}

#[test]
fn unreferenced_old_garbage_is_filtered_before_reading_its_stale_child() {
    let mut heap = heap();
    let garbage = heap.alloc_old(empty_pair()).unwrap();
    // SAFETY: the body is test-owned and no collection runs. This deliberately
    // stale, non-null offset must never be dereferenced: no heap edge names the
    // garbage holder, matching the verifier's existing garbage exclusion.
    let stale = RawGc((crate::compressed::cage_size() - crate::page::PAGE_SIZE) as u32);
    unsafe {
        inject_first(garbage, stale);
    }
    heap.verify_remembered_complete();
}

struct SharedPair {
    pair: RawGc,
}

impl Traceable for SharedPair {
    const TYPE_TAG: u8 = 0xef;

    unsafe fn trace_slots(this: *mut Self, visitor: &mut crate::raw::SlotVisitor<'_>) {
        // SAFETY: the body owns a live pair handle. Visit it first so a moving
        // collector updates it before the out-of-line slot is reached.
        unsafe {
            visitor(&raw mut (*this).pair);
            let body = (*this)
                .pair
                .as_header_ptr()
                .cast::<u8>()
                .add(std::mem::size_of::<GcHeader>())
                .cast::<OpaquePair>();
            visitor(&raw mut (*body).first);
        }
    }
}

#[test]
fn remembered_or_young_owners_cover_shared_old_storage_slots() {
    for old_owner in [true, false] {
        let mut heap = heap();
        let storage = heap.alloc_old(empty_pair()).unwrap();
        let owner = SharedPair {
            pair: storage.raw(),
        };
        let owner = if old_owner {
            heap.alloc_old(owner)
        } else {
            heap.alloc(owner)
        }
        .unwrap();
        let child = heap.alloc(OpaqueLeaf { payload: 817 }).unwrap();
        // SAFETY: a noncollecting test fault writes the live old storage slot.
        unsafe {
            inject_first(storage, child.raw());
        }
        if old_owner {
            heap.record_write_edge(owner, child.raw());
        }
        // SAFETY: no allocation or collection intervenes after these cells
        // were created and the optional owner barrier was recorded.
        unsafe {
            assert!(!(*storage.as_header_ptr()).is_remembered());
            assert!((*child.as_header_ptr()).is_young());
            assert_eq!((*owner.as_header_ptr()).is_young(), !old_owner);
            assert_eq!((*owner.as_header_ptr()).is_remembered(), old_owner);
        }
        heap.verify_remembered_complete();
        heap.read_payload(storage, |pair| assert_eq!(pair.first, child.raw()));
    }
}
