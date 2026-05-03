//! HandleScope RAII: nested scopes truncate on drop. Outer
//! scope's `Local`s remain valid; inner scope entries are
//! reclaimed when the inner scope drops.

use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, HandleScope};

struct Tag;
impl Traceable for Tag {
    const TYPE_TAG: u8 = 0x80;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

#[test]
fn nested_handle_scopes_truncate_on_drop() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Tag>();

    let g_outer = heap.alloc(Tag).unwrap();
    let outer = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let _outer_local = outer.local(g_outer);
    assert_eq!(heap.handle_stack().len(), 1);
    {
        let inner = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let g_inner = heap.alloc(Tag).unwrap();
        let _l1 = inner.local(g_inner);
        let _l2 = inner.local(g_inner);
        let _l3 = inner.local(g_inner);
        assert_eq!(heap.handle_stack().len(), 4);
    }
    assert_eq!(heap.handle_stack().len(), 1, "inner scope did not truncate");
    drop(outer);
    assert_eq!(heap.handle_stack().len(), 0);
}

#[test]
fn local_clone_creates_new_entry() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Tag>();
    let g = heap.alloc(Tag).unwrap();
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let l1 = scope.local(g);
    let _l2 = l1.clone();
    assert_eq!(heap.handle_stack().len(), 2);
}
