//! Task 94 external/backing-store accounting coverage.

#[test]
fn external_memory_reserves_resizes_and_releases() {
    let mut heap = otter_gc::GcHeap::with_max_heap_bytes(1024).expect("heap");

    let mut token = heap.reserve_external(128).expect("reserve");
    assert_eq!(token.bytes(), 128);
    assert_eq!(heap.tracked_bytes(), 128);

    token.resize(256).expect("grow");
    assert_eq!(token.bytes(), 256);
    assert_eq!(heap.tracked_bytes(), 256);

    token.resize(64).expect("shrink");
    assert_eq!(token.bytes(), 64);
    assert_eq!(heap.tracked_bytes(), 64);

    drop(token);
    assert_eq!(heap.tracked_bytes(), 0);
}

#[test]
fn external_memory_refuses_cap_overshoot_without_booking() {
    let mut heap = otter_gc::GcHeap::with_max_heap_bytes(64).expect("heap");

    assert!(heap.reserve_external(65).is_err());
    assert_eq!(heap.tracked_bytes(), 0);
}

#[test]
fn shared_external_memory_release_can_arrive_from_another_thread() {
    let mut heap = otter_gc::GcHeap::with_max_heap_bytes(1024).expect("heap");
    let mut roots = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
    let token = heap
        .reserve_shared_external_with_roots(256, &mut roots)
        .expect("reserve");
    assert_eq!(token.bytes(), 256);
    assert_eq!(heap.tracked_bytes(), 256);

    std::thread::spawn(move || drop(token))
        .join()
        .expect("join");

    assert_eq!(heap.tracked_bytes(), 0);
    let _next = heap.reserve_external(16).expect("reserve after drain");
    assert_eq!(heap.stats().reserved_bytes, 16);
    assert_eq!(heap.tracked_bytes(), 16);
}

#[test]
fn diagnostic_alloc_drains_shared_external_releases_before_recount() {
    let mut heap = otter_gc::GcHeap::with_max_heap_bytes(1024).expect("heap");
    let mut roots = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
    let token = heap
        .reserve_shared_external_with_roots(256, &mut roots)
        .expect("reserve");
    assert_eq!(heap.tracked_bytes(), 256);
    drop(token);

    let _leaf = heap
        .alloc_old_diagnostic(otter_gc::test_support::OpaqueLeaf { payload: 7 })
        .expect("diagnostic leaf");

    assert!(
        heap.tracked_bytes() < 256,
        "diagnostic recount must not retain released shared external bytes"
    );
    assert_eq!(heap.stats().reserved_bytes, 0);
}
