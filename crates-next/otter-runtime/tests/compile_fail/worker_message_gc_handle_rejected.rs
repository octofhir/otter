//! Worker messages must not carry raw GC handles.

fn main() {
    let worker = otter_runtime::Worker::new().unwrap();
    let handle: otter_gc::Gc<()> = otter_gc::Gc::null();
    worker.accepts_message(&handle);
}
