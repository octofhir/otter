//! Young-generation relocation for non-root weak registries.

use otter_gc::test_support::OpaqueLeaf;

#[test]
fn weak_ref_registry_updates_forwarded_young_handles() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let weak = heap.alloc(OpaqueLeaf { payload: 1 }).expect("weak");
    let original = weak.raw();
    heap.register_weak_ref(weak);

    let mut root = original;
    let mut roots = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        visitor(&mut root);
    };
    heap.collect_minor_with_roots(&mut roots);

    assert_eq!(heap.weak_refs_snapshot(), vec![root]);
    assert_ne!(root, original);
}

#[test]
fn weak_ref_registry_prunes_dead_young_handles() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let weak = heap.alloc(OpaqueLeaf { payload: 1 }).expect("weak");
    heap.register_weak_ref(weak);

    heap.collect_minor(otter_gc::EmptyRoots);

    assert_eq!(heap.weak_ref_count(), 0);
}

#[test]
fn ephemeron_registry_updates_forwarded_young_handles() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let table = heap.alloc(OpaqueLeaf { payload: 1 }).expect("table");
    let original = table.raw();
    heap.register_ephemeron_table(table);

    let mut root = original;
    let mut roots = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        visitor(&mut root);
    };
    heap.collect_minor_with_roots(&mut roots);

    assert_eq!(heap.ephemeron_tables_snapshot(), vec![root]);
    assert_ne!(root, original);
}

#[test]
fn ephemeron_registry_prunes_dead_young_handles() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let table = heap.alloc(OpaqueLeaf { payload: 1 }).expect("table");
    heap.register_ephemeron_table(table);

    heap.collect_minor(otter_gc::EmptyRoots);

    assert_eq!(heap.ephemeron_table_count(), 0);
}
