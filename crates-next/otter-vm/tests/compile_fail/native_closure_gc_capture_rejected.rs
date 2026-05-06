//! Public native closures must not hide raw GC handles in their
//! long-lived Rust payload.

fn main() {
    let mut heap = otter_gc::GcHeap::new().unwrap();
    let gc = heap
        .alloc(otter_gc::test_support::OpaqueLeaf { payload: 93 })
        .unwrap();

    let _ = otter_vm::native_value(&mut heap, "bad", move |_, _, _| {
        std::hint::black_box(gc);
        Ok(otter_vm::Value::Undefined)
    });
}
