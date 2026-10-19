//! Allocation colors during a marking cycle: young cells stay white, since
//! the cycle never marks them, and old cells are born black, since nothing
//! visits them; their allocator barriers the edges they start with.

use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, MarkColor};

struct Tag(#[allow(dead_code)] u64);
impl Traceable for Tag {
    const TYPE_TAG: u8 = 0x70;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

struct ConcurrentTag(#[allow(dead_code)] u64);
impl Traceable for ConcurrentTag {
    const TYPE_TAG: u8 = 0x71;
    const CONCURRENT_MARKING: bool = true;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

#[test]
fn old_allocation_during_marking_is_born_black() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Tag>();
    heap.register_traceable::<ConcurrentTag>();
    heap.marking_mut().start_cycle();
    let young = heap.alloc(Tag(0)).expect("young alloc during marking");
    let black = heap
        .alloc_old(ConcurrentTag(1))
        .expect("old alloc during marking");
    let other = heap.alloc_old(Tag(2)).expect("old alloc during marking");
    // SAFETY: the cells were just allocated and nothing collected since.
    unsafe {
        assert_eq!((*young.as_header_ptr()).mark_color(), MarkColor::White);
        assert!((*young.as_header_ptr()).is_young());
        assert_eq!((*black.as_header_ptr()).mark_color(), MarkColor::Black);
        assert_eq!((*other.as_header_ptr()).mark_color(), MarkColor::Black);
    }
    heap.marking_mut().finish_cycle();
}
