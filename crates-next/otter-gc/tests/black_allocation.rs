//! Black allocation: when a marking cycle is in progress the
//! allocator marks new objects black so the marker doesn't have
//! to re-discover them. The new object survives a finish-mark
//! sweep without any explicit shading.

use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, MarkColor};

struct Tag;
impl Traceable for Tag {
    const TYPE_TAG: u8 = 0x70;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

#[test]
fn allocation_during_marking_starts_black() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Tag>();
    // Manually drive a marking cycle.
    heap.marking_mut().start_cycle();
    let g = heap.alloc(Tag).expect("alloc during marking");
    // Header must already be black.
    let header = g.as_header_ptr();
    assert_eq!(unsafe { (*header).mark_color() }, MarkColor::Black);
    heap.marking_mut().finish_cycle();
}
