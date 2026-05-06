//! Persistent roots exposed outside `otter-gc` must go through
//! `Root<'iso, T>` plus a matching `GcSession`, not unbranded
//! `GlobalHandle<T>` construction.

fn main() {
    let mut heap = otter_gc::GcHeap::new().unwrap();

    let gc = heap
        .alloc(otter_gc::test_support::OpaqueLeaf { payload: 93 })
        .unwrap();
    let _root = heap.create_global(gc);
}
