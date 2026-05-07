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
