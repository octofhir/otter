//! Promotion: an object surviving one scavenge moves to old gen
//! on the next.

use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, HandleScope};

struct Tag;
impl Traceable for Tag {
    const TYPE_TAG: u8 = 0x50;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

#[test]
fn objects_promote_after_one_survival() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Tag>();
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let mut roots = Vec::new();
    for _ in 0..10 {
        let g = heap.alloc(Tag).unwrap();
        // Brand-new young.
        let header = g.as_header_ptr();
        assert!(unsafe { (*header).is_young() });
        roots.push(scope.local(g));
    }

    // First scavenge — copies them to to-space, bumps source page
    // survival_age. Since survival_age starts at 0 and the
    // scavenger increments to 1 after this scavenge, the second
    // scavenge reads survival_age == 1 ≥ PROMOTE_AFTER_SURVIVALS
    // and promotes.
    heap.collect_minor(otter_gc::EmptyRoots);
    // Second scavenge — survivors should land in old space.
    heap.collect_minor(otter_gc::EmptyRoots);

    for l in &roots {
        let g = l.get();
        let header = g.as_header_ptr();
        // After the second scavenge they live in old gen.
        assert!(
            unsafe { (*header).is_old() },
            "object did not promote (offset 0x{:x})",
            g.offset()
        );
    }
}
