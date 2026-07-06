//! Object-granular remembered set: an old → young pointer survives a
//! scavenge because the write barrier records the **parent object** in
//! the per-isolate store buffer, even when no other root references the
//! young child. The parent's `FLAG_REMEMBERED` bit is set by the barrier
//! and cleared by the scavenge that consumes the buffer; it is re-set
//! while the edge stays old → young and clears once the child promotes.
//!
//! Also covers the malloc-owned-slot shape: a traced slot living behind a
//! `Box` (outside the parent's heap cell) must still cause the parent to
//! be remembered. The remembered entry is the parent object, never the
//! slot, so off-page/exotic slots need no in-cage address — the scavenge
//! re-traces the whole parent and reaches the boxed slot through the
//! refreshed slab base.

use otter_gc::raw::RawGc;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{Gc, GcHeap, HandleScope};

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

/// Parent whose traced slot lives in malloc memory (behind a `Box`),
/// mirroring VM bodies like a parked frame's boxed register file.
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
fn old_to_young_pointer_survives_via_remembered_set() {
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
        // The parent must now be recorded in the remembered set.
        assert!(
            unsafe { (*promoted.as_header_ptr()).is_remembered() },
            "parent not remembered after barrier"
        );
        // Scope still holds parent rooted; child is reachable
        // ONLY through parent.child via the remembered set.
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
        "child reaped despite the remembered set"
    );

    // While the child stays YOUNG (evacuated to to-space, not yet
    // promoted), the scavenge re-records the parent — dropping it would
    // lose the old->young edge and dangle the child after its next move.
    // Once the child promotes (second scavenge), the edge is old->old and
    // the parent is left un-remembered.
    unsafe {
        let child_young = (*child_after.as_header_ptr()).is_young();
        assert_eq!(
            (*parent_header).is_remembered(),
            child_young,
            "remembered bit must track the child's generation"
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
        assert!(
            !(*parent_header).is_remembered(),
            "old->old edge leaves the parent un-remembered"
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
    // edge barrier must remember the *parent object* — the slot lives
    // off-cell and has no in-cage address to record.
    let child = heap.alloc(Box1 { child: Gc::null() }).unwrap();
    let child_local = scope.local(child);
    assert!(unsafe { (*child.as_header_ptr()).is_young() });
    let parent = heap
        .alloc_old(BoxedSlot {
            child: Box::new(child),
        })
        .unwrap();
    assert!(unsafe { (*parent.as_header_ptr()).is_old() });
    // The parent must be recorded in the remembered set.
    assert!(
        unsafe { (*parent.as_header_ptr()).is_remembered() },
        "alloc_old edge barrier must remember the parent for a boxed slot"
    );
    // Drop the direct root; the child stays reachable only through
    // the old parent's boxed slot via the remembered set.
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
    // A child reached through a remembered parent promotes on that very
    // scavenge (leaving it young would re-trace the parent on every
    // subsequent scavenge — quadratic for large promoted graphs), so the
    // old->young edge is gone and the parent must NOT stay remembered.
    assert!(
        unsafe { (*child_after.as_header_ptr()).is_old() },
        "dirty-retrace child must promote immediately"
    );
    assert!(
        !unsafe { (*parent.as_header_ptr()).is_remembered() },
        "promoted child leaves no old->young edge to re-remember"
    );

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
        assert!(
            !(*parent.as_header_ptr()).is_remembered(),
            "old->old boxed-slot edge leaves the parent un-remembered"
        );
    }
}

#[test]
fn old_slot_already_rewritten_to_to_space_re_remembers_parent() {
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
    // before the remembered parents are scanned. The root phase rewrites
    // parent.child to NewTo; the later remembered-set scan must still
    // re-record the old parent while that child remains young.
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
        assert!(
            (*parent.as_header_ptr()).is_remembered(),
            "old parent must be re-remembered when slot already points to NewTo"
        );
    }

    heap.collect_minor(otter_gc::EmptyRoots);
    let child_after_second = heap.read_payload(parent, |body| body.child);
    assert!(
        !child_after_second.is_null(),
        "child reaped after already-NewTo edge lost its remembered parent"
    );
    unsafe {
        assert_eq!(
            (*child_after_second.as_header_ptr()).type_tag(),
            Box1::TYPE_TAG
        );
    }
}
