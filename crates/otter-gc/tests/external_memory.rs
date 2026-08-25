//! Task 94 external/backing-store accounting coverage.

#[test]
fn external_memory_reserves_resizes_and_releases() {
    let mut heap = otter_gc::GcHeap::with_max_heap_bytes(1024).expect("heap");

    let mut token = heap.reserve_external(128).expect("reserve");
    assert_eq!(token.bytes(), 128);
    assert_eq!(heap.tracked_bytes(), 128);

    token.resize(&mut heap, 256).expect("grow");
    assert_eq!(token.bytes(), 256);
    assert_eq!(heap.tracked_bytes(), 256);

    token.resize(&mut heap, 64).expect("shrink");
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

#[test]
fn external_ledger_mirrors_reservations_limits_and_releases() {
    use otter_resource::{ResourceAccount, ResourceClass, ResourceLimits};

    let mut heap = otter_gc::GcHeap::with_max_heap_bytes(4096).expect("heap");
    let account = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::ExternalBytes, 256)
            .build(),
    );
    heap.set_external_bytes_account(&account)
        .expect("an empty heap installs against any budget");

    let mut token = heap.reserve_external(128).expect("reserve");
    let current = account
        .snapshot()
        .get(ResourceClass::ExternalBytes)
        .current();
    assert_eq!(current, 128);

    // Growth past the aggregate budget is a typed refusal before any heap
    // state changes; the existing charge stays exact.
    let error = token
        .resize(&mut heap, 512)
        .expect_err("budget refuses the growth");
    assert!(matches!(
        error,
        otter_gc::OutOfMemory::ExternalBudgetExceeded {
            requested_bytes: 384,
            in_use: 128,
            limit: 256,
        }
    ));
    assert_eq!(token.bytes(), 128);
    assert_eq!(heap.tracked_bytes(), 128);
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::ExternalBytes)
            .current(),
        128
    );
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::ExternalBytes)
            .rejections(),
        1
    );

    // Release is deferred through the shared channel; the next
    // reconciliation returns the ledger to baseline.
    token.release();
    heap.drain_external_releases();
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::ExternalBytes)
            .current(),
        0
    );
}

#[test]
fn external_ledger_shrinks_on_drained_shared_releases() {
    use otter_resource::{ResourceAccount, ResourceClass};

    let mut heap = otter_gc::GcHeap::with_max_heap_bytes(1024).expect("heap");
    let account = ResourceAccount::default();
    heap.set_external_bytes_account(&account)
        .expect("install unlimited account");

    let mut roots = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
    let token = heap
        .reserve_shared_external_with_roots(256, &mut roots)
        .expect("reserve");
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::ExternalBytes)
            .current(),
        256
    );

    std::thread::spawn(move || drop(token))
        .join()
        .expect("join");
    // The cross-thread release is deferred; the next booking drains it.
    let _next = heap.reserve_external(16).expect("reserve after drain");
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::ExternalBytes)
            .current(),
        16
    );
}

#[test]
fn installing_an_account_charges_outstanding_external_bytes() {
    use otter_resource::{ResourceAccount, ResourceClass, ResourceLimits};

    let mut heap = otter_gc::GcHeap::with_max_heap_bytes(1024).expect("heap");
    let _token = heap.reserve_external(200).expect("reserve");

    // A budget below the outstanding bytes refuses installation and leaves
    // the previous (absent) ledger in place.
    let small = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::ExternalBytes, 100)
            .build(),
    );
    assert!(heap.set_external_bytes_account(&small).is_err());
    assert_eq!(
        small.snapshot().get(ResourceClass::ExternalBytes).current(),
        0
    );

    let account = ResourceAccount::default();
    heap.set_external_bytes_account(&account)
        .expect("unlimited install");
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::ExternalBytes)
            .current(),
        200
    );
}
