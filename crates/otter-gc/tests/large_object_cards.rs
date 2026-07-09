//! Large-object-space remembered set: a large object's header
//! reports old, so the generational write barrier marks cards on
//! its LOS page — and the scavenger must SCAN those cards, or an
//! old→young edge minted by `large.child = young` dangles after
//! the child moves on the next scavenge.

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

/// Payload fat enough to land in large-object space
/// (> LARGE_OBJECT_THRESHOLD = half a page).
struct Big {
    child: Gc<Small>,
    _ballast: [u8; 192 * 1024],
}

impl Traceable for Big {
    const TYPE_TAG: u8 = 0x63;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        unsafe {
            let slot = std::ptr::addr_of_mut!((*this).child) as *mut RawGc;
            v(slot);
        }
    }
}

#[test]
fn large_object_old_to_young_edge_survives_scavenge() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Small>();
    heap.register_traceable::<Big>();

    let big = heap
        .alloc(Big {
            child: Gc::null(),
            _ballast: [0u8; 192 * 1024],
        })
        .expect("large alloc");
    assert!(
        unsafe { (*big.as_header_ptr()).is_old() },
        "large objects must report old for the generational barrier"
    );

    // Young child referenced ONLY through the large object.
    let child = heap.alloc(Small { child: Gc::null() }).expect("child");
    assert!(unsafe { (*child.as_header_ptr()).is_young() });
    unsafe {
        let payload = (big.as_header_ptr() as *mut u8)
            .add(std::mem::size_of::<otter_gc::GcHeader>()) as *mut Big;
        (*payload).child = child;
    }
    heap.write_barrier(big, child);

    // Scavenge: the child must survive (and the slot must follow it)
    // purely via the LOS dirty-card scan.
    heap.collect_minor(otter_gc::EmptyRoots).expect("minor GC");

    let child_after = unsafe {
        let payload = (big.as_header_ptr() as *mut u8)
            .add(std::mem::size_of::<otter_gc::GcHeader>()) as *mut Big;
        (*payload).child
    };
    assert!(!child_after.is_null(), "edge lost after scavenge");
    // The relocated child must be a readable Small whose header
    // carries the right type tag.
    unsafe {
        let header = child_after.as_header_ptr();
        assert_eq!((*header).type_tag(), Small::TYPE_TAG);
    }
}
