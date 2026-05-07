//! A weak handle branded by one isolate must not be upgraded through
//! another isolate's session.

fn main() {
    let mut left = otter_gc::GcHeap::new().unwrap();
    let mut right = otter_gc::GcHeap::new().unwrap();

    otter_gc::with_gc_session(&mut left, |left_session| {
        let weak = left_session.weak(otter_gc::Gc::<otter_gc::test_support::OpaqueLeaf>::null());

        otter_gc::with_gc_session(&mut right, |right_session| {
            let _ = weak.upgrade(&right_session);
        });
    });
}
