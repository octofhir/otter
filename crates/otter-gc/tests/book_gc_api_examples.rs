//! Buildable examples backing `docs/book/src/engine/gc-api.md`.

use otter_gc::test_support::OpaqueLeaf;
use otter_gc::{EscapableHandleScope, GcHeap, HandleScope, with_gc_session};

#[test]
fn book_example_escapes_one_local_from_nested_scope() {
    let mut heap = GcHeap::new().unwrap();
    heap.register_traceable::<OpaqueLeaf>();
    let gc = heap.alloc(OpaqueLeaf { payload: 95 }).unwrap();
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
fn book_example_accounts_external_memory_with_raii() {
    let mut heap = GcHeap::with_max_heap_bytes(4096).unwrap();

    let mut backing = heap.reserve_external(1024).unwrap();
    assert_eq!(backing.bytes(), 1024);

    backing.resize(2048).unwrap();
    assert_eq!(backing.bytes(), 2048);

    backing.release();
    assert_eq!(heap.tracked_bytes(), 0);
}

#[test]
fn book_example_uses_branded_root_and_weak_handles() {
    let mut heap = GcHeap::new().unwrap();
    heap.register_traceable::<OpaqueLeaf>();

    with_gc_session(&mut heap, |mut session| {
        let gc = session.alloc(OpaqueLeaf { payload: 95 }).unwrap();
        let root = session.root(gc);
        let weak = session.weak(root.get(&session));

        assert_eq!(root.get(&session).offset(), gc.offset());
        assert_eq!(weak.upgrade(&session).unwrap().offset(), gc.offset());
    });
}
