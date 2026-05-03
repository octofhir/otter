//! Card-table remembered set: an old → young pointer survives a
//! scavenge via the dirty-card scan, even when no other root
//! references the young object. The card returns to clean after
//! the scavenge.

use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{Gc, GcHeap, HandleScope, Page, RawGc};

struct Box1 {
    child: Gc<Box1>,
}

impl Traceable for Box1 {
    const TYPE_TAG: u8 = 0x60;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        unsafe {
            let slot = std::ptr::addr_of_mut!((*this).child) as *mut RawGc;
            v(slot);
        }
    }
}

#[test]
fn old_to_young_pointer_survives_via_dirty_card() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Box1>();

    // Allocate parent, promote it to old by running two
    // scavenges with it rooted.
    let parent_offset_after_promotion;
    {
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let p = heap.alloc(Box1 { child: Gc::null() }).unwrap();
        let local = scope.local(p);
        heap.collect_minor(otter_gc::EmptyRoots);
        heap.collect_minor(otter_gc::EmptyRoots);
        // p has been promoted to old space; its handle reflects
        // the new offset.
        let promoted = local.get();
        assert!(unsafe { (*promoted.as_header_ptr()).is_old() });
        parent_offset_after_promotion = promoted.offset();

        // Allocate a fresh young child and store via barrier.
        let child = heap.alloc(Box1 { child: Gc::null() }).unwrap();
        assert!(unsafe { (*child.as_header_ptr()).is_young() });
        // Write parent.child = child. Need pointer to the slot.
        unsafe {
            let parent_payload = (promoted.as_header_ptr() as *mut u8)
                .add(std::mem::size_of::<otter_gc::GcHeader>())
                as *mut Box1;
            let slot_addr = std::ptr::addr_of_mut!((*parent_payload).child);
            (*slot_addr) = child;
            // Fire the barrier.
            heap.write_barrier(promoted, slot_addr, child);
        }
        // The page containing the slot must now have a dirty
        // card.
        unsafe {
            let parent_header_ptr = promoted.as_header_ptr() as *const u8;
            let page_header = Page::header_of(parent_header_ptr);
            let page_base = Page::page_base_of(parent_header_ptr);
            let parent_payload_addr =
                parent_header_ptr as usize + std::mem::size_of::<otter_gc::GcHeader>();
            let slot_byte_offset = parent_payload_addr - (page_base as usize);
            assert!(
                page_header.is_card_dirty(slot_byte_offset),
                "card not dirty after barrier"
            );
        }
        // Scope still holds parent rooted; child is reachable
        // ONLY through parent.child via the dirty card.
        let _ = local;
        // Hold parent via global handle so it survives scope
        // close.
        let _g = heap.create_global(promoted);
        std::mem::forget(_g); // leak intentionally so we can
        // verify post-scavenge state.
    }
    // Run a scavenge.
    heap.collect_minor(otter_gc::EmptyRoots);

    // Re-acquire the parent (its offset is stable — old gen).
    let parent: Gc<Box1> = unsafe { Gc::from_offset(parent_offset_after_promotion) };
    let parent_header = parent.as_header_ptr();
    assert!(unsafe { (*parent_header).is_old() });
    // Read parent.child — must be non-null and live.
    let child_after = unsafe {
        let payload =
            (parent_header as *mut u8).add(std::mem::size_of::<otter_gc::GcHeader>()) as *mut Box1;
        (*payload).child
    };
    assert!(
        !child_after.is_null(),
        "child reaped despite dirty-card remembered set"
    );

    // Scavenge cleared the card (rewritten by the scan).
    unsafe {
        let page_header = Page::header_of(parent_header as *const u8);
        let page_base = Page::page_base_of(parent_header as *const u8);
        let parent_payload_addr =
            parent_header as usize + std::mem::size_of::<otter_gc::GcHeader>();
        let slot_byte_offset = parent_payload_addr - (page_base as usize);
        assert!(
            !page_header.is_card_dirty(slot_byte_offset),
            "card still dirty after scavenge"
        );
    }
}
