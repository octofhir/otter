//! Worker messages must not carry branded persistent roots.

fn main() {
    let worker = otter_runtime::Worker::new().unwrap();
    let mut heap = otter_gc::GcHeap::new().unwrap();

    otter_gc::with_gc_session(&mut heap, |session| {
        let root = session.root(otter_gc::Gc::<otter_gc::test_support::OpaqueLeaf>::null());
        worker.accepts_message(&root);
    });
}
