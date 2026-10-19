//! Young large-object space: a large body is born young on its own page,
//! is promoted in place (its address kept, its page moved to the old large
//! object space) by the scavenge that finds it reachable, and is released
//! with its drop glue run by the scavenge that does not. Once old, the
//! generational barrier remembers it, so an old→young edge minted by
//! `large.child = young` survives the next scavenge.

use std::sync::Arc;

use otter_gc::raw::RawGc;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{Gc, GcHeap};

struct Small {
    child: Gc<Small>,
}

impl Traceable for Small {
    const TYPE_TAG: u8 = 0x62;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        unsafe {
            let slot = std::ptr::addr_of_mut!((*this).child) as *mut RawGc;
            v(slot);
        }
    }
}

/// Body whose trailing ballast is fat enough to land in large-object
/// space (> LARGE_OBJECT_THRESHOLD = half a page). The ballast trails the
/// body so the allocator never moves it by value. `owner` tells whether
/// the body's drop glue ran.
struct Big {
    child: Gc<Small>,
    _owner: Arc<()>,
}

const BALLAST_BYTES: usize = 192 * 1024;

impl Traceable for Big {
    const TYPE_TAG: u8 = 0x63;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        unsafe {
            let slot = std::ptr::addr_of_mut!((*this).child) as *mut RawGc;
            v(slot);
        }
    }
}

fn payload(big: Gc<Big>) -> *mut Big {
    // SAFETY: `big` is a live large cell; its payload follows the header.
    unsafe {
        (big.as_header_ptr() as *mut u8).add(std::mem::size_of::<otter_gc::GcHeader>()) as *mut Big
    }
}

fn heap() -> GcHeap {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Small>();
    heap.register_traceable::<Big>();
    heap
}

#[test]
fn unreachable_young_large_body_is_released_with_its_drop_glue() {
    let mut heap = heap();
    let owner = Arc::new(());
    let big = heap
        .alloc_trailing_with_roots(
            Big {
                child: Gc::null(),
                _owner: Arc::clone(&owner),
            },
            BALLAST_BYTES,
            &mut |_| {},
        )
        .expect("large alloc");
    // SAFETY: freshly allocated cell.
    assert!(unsafe { (*big.as_header_ptr()).is_young() });
    assert_eq!(heap.census().young_large.object_count, 1);
    assert_eq!(Arc::strong_count(&owner), 2);

    heap.collect_minor(otter_gc::EmptyRoots).expect("minor GC");

    let census = heap.census();
    assert_eq!(census.young_large.object_count, 0);
    assert_eq!(census.large.object_count, 0);
    assert_eq!(
        Arc::strong_count(&owner),
        1,
        "the released body was dropped"
    );
}

#[test]
fn reachable_young_large_body_is_promoted_in_place_and_keeps_its_edges() {
    let mut heap = heap();
    let big = heap
        .alloc_trailing_with_roots(
            Big {
                child: Gc::null(),
                _owner: Arc::new(()),
            },
            BALLAST_BYTES,
            &mut |_| {},
        )
        .expect("large alloc");
    // A young child reachable only through the young large body.
    let child = heap.alloc(Small { child: Gc::null() }).expect("child");
    // SAFETY: both cells are live; a young parent needs no barrier.
    unsafe { (*payload(big)).child = child };

    let original = big.raw();
    let mut root = original;
    let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| visitor(&mut root);
    heap.collect_minor_with_roots(&mut roots).expect("minor GC");

    assert_eq!(root, original, "a large body is never copied");
    // SAFETY: the root kept the cell alive at its address.
    unsafe {
        assert!((*big.as_header_ptr()).is_old(), "promoted in place");
        let moved = (*payload(big)).child;
        assert!(!moved.is_null());
        assert_ne!(moved.raw(), child.raw(), "the child left the nursery");
        assert_eq!((*moved.as_header_ptr()).type_tag(), Small::TYPE_TAG);
    }
    let census = heap.census();
    assert_eq!(census.young_large.object_count, 0);
    assert_eq!(census.large.object_count, 1);

    // Now old: a fresh young child published through the barrier survives
    // the next scavenge through the remembered parent alone.
    let fresh = heap.alloc(Small { child: Gc::null() }).expect("fresh");
    // SAFETY: both cells are live.
    unsafe { (*payload(big)).child = fresh };
    heap.write_barrier(big, fresh);
    let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| visitor(&mut root);
    heap.collect_minor_with_roots(&mut roots).expect("minor GC");
    // SAFETY: the root kept the cell alive.
    unsafe {
        let after = (*payload(big)).child;
        assert!(!after.is_null(), "edge lost after scavenge");
        assert_ne!(after.raw(), fresh.raw());
        assert_eq!((*after.as_header_ptr()).type_tag(), Small::TYPE_TAG);
    }
}

#[test]
fn young_large_bodies_past_the_nursery_capacity_scavenge_first() {
    let mut heap = heap();
    let before = heap.gc_stats().minor_gc_cycles;
    // Unrooted bodies: each scavenge the capacity forces releases them all.
    for _ in 0..512 {
        heap.alloc_trailing_with_roots(
            Big {
                child: Gc::null(),
                _owner: Arc::new(()),
            },
            BALLAST_BYTES,
            &mut |_| {},
        )
        .expect("large alloc");
    }
    assert!(heap.gc_stats().minor_gc_cycles > before);
    assert!(heap.census().young_large.page_count < 512);
}
