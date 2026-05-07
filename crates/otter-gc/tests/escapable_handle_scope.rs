//! Regression tests for the contributor-facing escapable handle tier.

use otter_gc::test_support::OpaqueLeaf;
use otter_gc::{EscapableHandleScope, GcHeap, HandleScope};

#[test]
fn escapable_scope_preserves_one_local_in_parent_scope() {
    let mut heap = GcHeap::new().unwrap();
    heap.register_traceable::<OpaqueLeaf>();
    let gc = heap.alloc(OpaqueLeaf { payload: 94 }).unwrap();
    let stack = heap.handle_stack();

    let parent = HandleScope::new(stack);
    let escaped = {
        let mut inner = EscapableHandleScope::new(stack);
        let local = inner.local(gc);
        inner.escape(&local)
    };

    assert_eq!(escaped.get().offset(), gc.offset());
    assert_eq!(parent.local(gc).get().offset(), gc.offset());
}

#[test]
#[should_panic(expected = "EscapableHandleScope::escape called twice")]
fn escapable_scope_rejects_second_escape() {
    let mut heap = GcHeap::new().unwrap();
    heap.register_traceable::<OpaqueLeaf>();
    let gc = heap.alloc(OpaqueLeaf { payload: 95 }).unwrap();
    let stack = heap.handle_stack();

    let mut inner = EscapableHandleScope::new(stack);
    let first = inner.local(gc);
    let second = inner.local(gc);
    let _ = inner.escape(&first);
    let _ = inner.escape(&second);
}
