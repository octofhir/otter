//! Root-aware allocation contract: callers that hold live slots
//! outside the heap's handle/global tables can expose them to
//! allocation-triggered collection.

use otter_gc::raw::RawGc;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{Gc, GcHeap};

struct Block {
    _payload: [u8; 3 * 1024],
}

struct Leaf {
    _payload: [u8; 3 * 1024],
}

struct Holder {
    child: Gc<Leaf>,
    _payload: [u8; 3 * 1024],
}

impl Traceable for Block {
    const TYPE_TAG: u8 = 0x35;

    unsafe fn trace_slots(_this: *mut Self, _visitor: &mut SlotVisitor<'_>) {}
}

impl Traceable for Leaf {
    const TYPE_TAG: u8 = 0x36;

    unsafe fn trace_slots(_this: *mut Self, _visitor: &mut SlotVisitor<'_>) {}
}

impl Traceable for Holder {
    const TYPE_TAG: u8 = 0x37;

    unsafe fn trace_slots(this: *mut Self, visitor: &mut SlotVisitor<'_>) {
        unsafe {
            visitor(std::ptr::addr_of_mut!((*this).child) as *mut RawGc);
        }
    }
}

#[test]
fn alloc_with_roots_preserves_external_stack_slot_during_emergency_gc() {
    let mut heap = GcHeap::with_max_heap_bytes(8 * 1024).expect("heap");

    let mut rooted: Gc<Block> = heap
        .alloc(Block {
            _payload: [0; 3 * 1024],
        })
        .expect("first block fits");
    let _unrooted = heap
        .alloc(Block {
            _payload: [0; 3 * 1024],
        })
        .expect("second block fits");

    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        let slot = std::ptr::addr_of_mut!(rooted) as *mut RawGc;
        visitor(slot);
    };

    let third = heap.alloc_with_roots(
        Block {
            _payload: [0; 3 * 1024],
        },
        &mut external_visit,
    );
    assert!(
        third.is_ok(),
        "third block should fit after collecting only the unrooted block"
    );
    assert!(
        !rooted.is_null(),
        "external root slot must remain populated"
    );
    assert!(
        heap.tracked_bytes() > 5 * 1024,
        "rooted first block and third block should both remain accounted"
    );
}

#[test]
fn alloc_with_roots_traces_pending_payload_during_emergency_gc() {
    let mut heap = GcHeap::with_max_heap_bytes(8 * 1024).expect("heap");

    let leaf = heap
        .alloc(Leaf {
            _payload: [0; 3 * 1024],
        })
        .expect("leaf fits");
    let _unrooted = heap
        .alloc(Block {
            _payload: [0; 3 * 1024],
        })
        .expect("block fits");

    let holder = heap.alloc_with_roots(
        Holder {
            child: leaf,
            _payload: [0; 3 * 1024],
        },
        &mut |_| {},
    );
    assert!(
        holder.is_ok(),
        "holder should fit after collecting only the unrelated block"
    );
    assert!(
        heap.tracked_bytes() > 5 * 1024,
        "pending holder payload should keep its child live through emergency GC"
    );
}

#[test]
fn reserve_bytes_with_roots_preserves_external_stack_slot_during_emergency_gc() {
    let mut heap = GcHeap::with_max_heap_bytes(8 * 1024).expect("heap");

    let mut rooted: Gc<Leaf> = heap
        .alloc(Leaf {
            _payload: [0; 3 * 1024],
        })
        .expect("leaf fits");
    let _unrooted = heap
        .alloc(Block {
            _payload: [0; 3 * 1024],
        })
        .expect("block fits");

    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        let slot = std::ptr::addr_of_mut!(rooted) as *mut RawGc;
        visitor(slot);
    };
    heap.reserve_bytes_with_roots(3 * 1024, &mut external_visit)
        .expect("reservation should fit after collecting only the unrooted block");

    assert!(
        !rooted.is_null(),
        "external root slot must remain populated"
    );
    assert!(
        heap.tracked_bytes() > 5 * 1024,
        "rooted leaf and reservation should both remain accounted"
    );
}
