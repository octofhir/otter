//! A root branded by one isolate must not be read through another
//! isolate's session.

fn main() {
    let mut left = otter_gc::GcHeap::new().unwrap();
    let mut right = otter_gc::GcHeap::new().unwrap();

    otter_gc::with_gc_session(&mut left, |mut left_session| {
        let root = left_session.root(otter_gc::Gc::<otter_gc::test_support::OpaqueLeaf>::null());

        otter_gc::with_gc_session(&mut right, |right_session| {
            let _ = root.get(&right_session);
        });
    });
}
