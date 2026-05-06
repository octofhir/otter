//! Trace table: register two type tags, allocate one of each,
//! verify a full GC traces correctly via the registered fns.

use otter_gc::raw::RawGc;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{Gc, GcHeap, HandleScope};

struct Leaf;
impl Traceable for Leaf {
    const TYPE_TAG: u8 = 0x90;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

struct Node {
    next: Gc<Leaf>,
}
impl Traceable for Node {
    const TYPE_TAG: u8 = 0x91;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        unsafe {
            let slot = std::ptr::addr_of_mut!((*this).next) as *mut RawGc;
            v(slot);
        }
    }
}

#[test]
fn full_gc_walks_through_registered_trace_fns() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Leaf>();
    heap.register_traceable::<Node>();

    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let leaf = heap.alloc(Leaf).unwrap();
    let node = heap.alloc(Node { next: leaf }).unwrap();
    let root = scope.local(node);
    // Promote both to old gen so the full mark phase walks
    // them.
    heap.collect_minor(otter_gc::EmptyRoots);
    heap.collect_minor(otter_gc::EmptyRoots);
    heap.collect_full(&mut |_| {});
    // Both still reachable: leaf via node.next, node via root.
    let n_after = root.get();
    let leaf_after = unsafe {
        let payload = (n_after.as_header_ptr() as *mut u8)
            .add(std::mem::size_of::<otter_gc::GcHeader>()) as *mut Node;
        (*payload).next
    };
    assert!(!leaf_after.is_null());
    let leaf_header = leaf_after.as_header_ptr();
    assert_eq!(
        unsafe { (*leaf_header).type_tag() },
        Leaf::TYPE_TAG,
        "wrong tag at traced leaf"
    );
}
