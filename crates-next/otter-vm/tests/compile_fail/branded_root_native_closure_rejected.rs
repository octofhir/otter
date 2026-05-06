//! Native closures stored across turns must not capture branded roots
//! directly.

fn store_native_callback<F: FnOnce() + Send + 'static>(_callback: F) {}

fn main() {
    let mut heap = otter_gc::GcHeap::new().unwrap();

    otter_gc::with_gc_session(&mut heap, |mut session| {
        let gc = session
            .alloc(otter_gc::test_support::OpaqueLeaf { payload: 93 })
            .unwrap();
        let root = session.root(gc);

        store_native_callback(move || {
            std::hint::black_box(root);
        });
    });
}
