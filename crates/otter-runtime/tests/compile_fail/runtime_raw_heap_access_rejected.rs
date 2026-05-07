//! Public runtime embedders must not get raw `GcHeap` access.

fn main() {
    let mut runtime = otter_runtime::Runtime::builder().build().unwrap();
    let _ = runtime.gc_heap_mut();
}
