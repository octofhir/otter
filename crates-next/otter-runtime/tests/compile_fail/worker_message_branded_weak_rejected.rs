//! Worker messages must not carry branded weak handles.

fn main() {
    let worker = otter_runtime::Worker::new().unwrap();
    let mut heap = otter_gc::GcHeap::new().unwrap();

    otter_gc::with_gc_session(&mut heap, |session| {
        let weak = session.weak(otter_gc::Gc::<otter_gc::test_support::OpaqueLeaf>::null());
        worker.accepts_message(&weak);
    });
}
