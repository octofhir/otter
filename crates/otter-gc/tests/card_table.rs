//! Card-table remembered set: an old → young pointer survives a
//! scavenge via the dirty-card scan, even when no other root
//! references the young object. The card returns to clean after
//! the scavenge.
//!
//! Also covers the malloc-owned-slot shape: a traced slot living
//! behind a `Box` (outside the parent's heap cell) must still
//! produce a dirty card on the **parent's** page. Computing the
//! card from the slot address would fabricate a page header in
//! malloc memory — a wild single-bit write — and the young child
//! would be reaped because the real parent page stays clean.

use otter_gc::raw::RawGc;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{Gc, GcHeap, HandleScope, Page};

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

/// Parent whose traced slot lives in malloc memory (behind a
/// `Box`), mirroring VM bodies like a parked frame's boxed
/// register file.
struct BoxedSlot {
    child: Box<Gc<Box1>>,
}

impl Traceable for BoxedSlot {
    const TYPE_TAG: u8 = 0x61;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        unsafe {
            let slot = (&mut *(*this).child) as *mut Gc<Box1> as *mut RawGc;
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
            heap.write_barrier(promoted, child);
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
        // Hold parent via branded root so it survives scope close.
        // The intentional leak keeps the root live for the
        // post-scavenge assertions without exposing unbranded
        // `GlobalHandle` construction to external callers.
        otter_gc::with_gc_session(&mut heap, |mut session| {
            let root = session.root(promoted);
            std::mem::forget(root);
        });
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

    // While the child stays YOUNG (evacuated to to-space, not yet
    // promoted), the scavenge re-dirties the slot's card — dropping
    // it would lose the old->young edge and dangle the child after
    // its next move. Once the child promotes (second scavenge), the
    // edge is old->old and the card stays clean.
    unsafe {
        let page_header = Page::header_of(parent_header as *const u8);
        let page_base = Page::page_base_of(parent_header as *const u8);
        let parent_payload_addr =
            parent_header as usize + std::mem::size_of::<otter_gc::GcHeader>();
        let slot_byte_offset = parent_payload_addr - (page_base as usize);
        let child_young = (*child_after.as_header_ptr()).is_young();
        assert_eq!(
            page_header.is_card_dirty(slot_byte_offset),
            child_young,
            "card dirtiness must track the child's generation"
        );
    }
    heap.collect_minor(otter_gc::EmptyRoots);
    let child_promoted = unsafe {
        let payload =
            (parent_header as *mut u8).add(std::mem::size_of::<otter_gc::GcHeader>()) as *mut Box1;
        (*payload).child
    };
    assert!(!child_promoted.is_null());
    unsafe {
        assert!(
            (*child_promoted.as_header_ptr()).is_old(),
            "child promotes on its second scavenge"
        );
        let page_header = Page::header_of(parent_header as *const u8);
        let page_base = Page::page_base_of(parent_header as *const u8);
        let parent_payload_addr =
            parent_header as usize + std::mem::size_of::<otter_gc::GcHeader>();
        let slot_byte_offset = parent_payload_addr - (page_base as usize);
        assert!(
            !page_header.is_card_dirty(slot_byte_offset),
            "old->old edge keeps the card clean"
        );
    }
}

#[test]
fn old_to_young_pointer_behind_boxed_slot_survives_scavenge() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Box1>();
    heap.register_traceable::<BoxedSlot>();

    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    // Young child, then an old parent whose only reference to the
    // child sits behind a Box (malloc memory). `alloc_old`'s payload
    // edge barrier must mark the card of the *parent header* — the
    // slot address is useless for card math.
    let child = heap.alloc(Box1 { child: Gc::null() }).unwrap();
    let child_local = scope.local(child);
    assert!(unsafe { (*child.as_header_ptr()).is_young() });
    let parent = heap
        .alloc_old(BoxedSlot {
            child: Box::new(child),
        })
        .unwrap();
    assert!(unsafe { (*parent.as_header_ptr()).is_old() });
    // The parent's page must carry the dirty card.
    unsafe {
        let parent_addr = parent.as_header_ptr() as *const u8;
        let page_header = Page::header_of(parent_addr);
        let page_base = Page::page_base_of(parent_addr);
        let byte_offset = parent_addr as usize - page_base as usize;
        assert!(
            page_header.is_card_dirty(byte_offset),
            "alloc_old edge barrier must dirty the parent's card for a boxed slot"
        );
    }
    // Drop the direct root; the child stays reachable only through
    // the old parent's boxed slot via the dirty card.
    let _ = child_local;
    drop(scope);
    otter_gc::with_gc_session(&mut heap, |mut session| {
        let root = session.root(parent);
        std::mem::forget(root);
    });
    heap.collect_minor(otter_gc::EmptyRoots);
    let child_after = heap.read_payload(parent, |body| *body.child);
    assert!(
        !child_after.is_null(),
        "boxed-slot child reaped despite alloc_old edge barrier"
    );
    // The forwarded child must still carry its registered type tag.
    assert_eq!(
        unsafe { (*child_after.as_header_ptr()).type_tag() },
        Box1::TYPE_TAG,
        "boxed-slot child header clobbered after scavenge"
    );
    unsafe {
        let parent_addr = parent.as_header_ptr() as *const u8;
        let page_header = Page::header_of(parent_addr);
        let page_base = Page::page_base_of(parent_addr);
        let byte_offset = parent_addr as usize - page_base as usize;
        assert!(
            page_header.is_card_dirty(byte_offset),
            "boxed-slot edge must re-dirty the parent card while the child remains young"
        );
    }

    heap.collect_minor(otter_gc::EmptyRoots);
    let child_after_second = heap.read_payload(parent, |body| *body.child);
    assert!(
        !child_after_second.is_null(),
        "boxed-slot child reaped after the second scavenge"
    );
    assert_eq!(
        unsafe { (*child_after_second.as_header_ptr()).type_tag() },
        Box1::TYPE_TAG,
        "boxed-slot child header clobbered after second scavenge"
    );
    unsafe {
        assert!(
            (*child_after_second.as_header_ptr()).is_old(),
            "child promotes on its second scavenge"
        );
        let parent_addr = parent.as_header_ptr() as *const u8;
        let page_header = Page::header_of(parent_addr);
        let page_base = Page::page_base_of(parent_addr);
        let byte_offset = parent_addr as usize - page_base as usize;
        assert!(
            !page_header.is_card_dirty(byte_offset),
            "old->old boxed-slot edge keeps the parent card clean"
        );
    }
}

#[test]
fn old_slot_already_rewritten_to_to_space_redirties_parent_card() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Box1>();

    let parent_offset_after_promotion;
    let child_slot;
    {
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let p = heap.alloc(Box1 { child: Gc::null() }).unwrap();
        let local = scope.local(p);
        heap.collect_minor(otter_gc::EmptyRoots);
        heap.collect_minor(otter_gc::EmptyRoots);
        let promoted = local.get();
        assert!(unsafe { (*promoted.as_header_ptr()).is_old() });
        parent_offset_after_promotion = promoted.offset();

        let child = heap.alloc(Box1 { child: Gc::null() }).unwrap();
        assert!(unsafe { (*child.as_header_ptr()).is_young() });
        unsafe {
            let parent_payload = (promoted.as_header_ptr() as *mut u8)
                .add(std::mem::size_of::<otter_gc::GcHeader>())
                as *mut Box1;
            let slot_addr = std::ptr::addr_of_mut!((*parent_payload).child);
            (*slot_addr) = child;
            child_slot = slot_addr as *mut RawGc;
            heap.write_barrier(promoted, child);
        }
        otter_gc::with_gc_session(&mut heap, |mut session| {
            let root = session.root(promoted);
            std::mem::forget(root);
        });
    }

    // Simulate an external root provider that visits an interior slot
    // before dirty cards are scanned. The root phase rewrites
    // parent.child to NewTo; the later card scan must still re-dirty
    // the old parent while that child remains young.
    let mut external = |visit: &mut dyn FnMut(*mut RawGc)| {
        visit(child_slot);
    };
    heap.collect_minor_with_roots(&mut external);

    let parent: Gc<Box1> = unsafe { Gc::from_offset(parent_offset_after_promotion) };
    let child_after = heap.read_payload(parent, |body| body.child);
    assert!(
        !child_after.is_null(),
        "child reaped despite external-root slot rewrite"
    );
    unsafe {
        assert!(
            (*child_after.as_header_ptr()).is_young(),
            "child should still be young after one scavenge"
        );
        let parent_addr = parent.as_header_ptr() as *const u8;
        let page_header = Page::header_of(parent_addr);
        let page_base = Page::page_base_of(parent_addr);
        let byte_offset = parent_addr as usize - page_base as usize;
        assert!(
            page_header.is_card_dirty(byte_offset),
            "old parent card must be re-dirtied when slot already points to NewTo"
        );
    }

    heap.collect_minor(otter_gc::EmptyRoots);
    let child_after_second = heap.read_payload(parent, |body| body.child);
    assert!(
        !child_after_second.is_null(),
        "child reaped after already-NewTo edge lost its remembered card"
    );
    unsafe {
        assert_eq!(
            (*child_after_second.as_header_ptr()).type_tag(),
            Box1::TYPE_TAG
        );
    }
}
