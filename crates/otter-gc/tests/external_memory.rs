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

/// Old-space slot whose only payload is an external reservation: the shape
/// of a JavaScript `ArrayBuffer` body.
struct ExternalHolder {
    _token: otter_gc::ExternalMemory,
}

impl otter_gc::trace::Traceable for ExternalHolder {
    const TYPE_TAG: u8 = 0x42;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut otter_gc::trace::SlotVisitor<'_>) {}
}

const HOLDER_BYTES: u64 = 4 * 1024 * 1024;
/// The major-GC budget floor: with no surviving pages or reservations the
/// budget sits at this floor, so dead payloads can accumulate to it plus one
/// in-flight holder before the growth-triggered collection reclaims them.
const MAJOR_GC_FLOOR_BYTES: u64 = 16 * 1024 * 1024;

fn churn_dead_holders(heap: &mut otter_gc::GcHeap, count: usize) -> u64 {
    heap.register_traceable::<ExternalHolder>();
    let mut peak_reserved = 0;
    for _ in 0..count {
        let token = heap.reserve_external(HOLDER_BYTES).expect("reserve");
        let _dead = heap
            .alloc_old(ExternalHolder { _token: token })
            .expect("alloc holder");
        peak_reserved = peak_reserved.max(heap.stats().reserved_bytes);
    }
    peak_reserved
}

#[test]
fn dead_external_payloads_trigger_major_gc_without_a_cap() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let churned = 128 * HOLDER_BYTES;
    let peak = churn_dead_holders(&mut heap, 128);
    assert!(
        peak <= MAJOR_GC_FLOOR_BYTES + HOLDER_BYTES,
        "dead external bytes peaked at {peak} of {churned} churned"
    );
}

#[test]
fn dead_external_payloads_trigger_major_gc_below_the_cap() {
    let mut heap = otter_gc::GcHeap::with_max_heap_bytes(2 * 1024 * 1024 * 1024).expect("heap");
    let peak = churn_dead_holders(&mut heap, 128);
    assert!(
        peak <= MAJOR_GC_FLOOR_BYTES + HOLDER_BYTES,
        "dead external bytes peaked at {peak} below a 2 GiB cap"
    );
    assert!(
        heap.tracked_bytes() <= MAJOR_GC_FLOOR_BYTES + HOLDER_BYTES,
        "tracked bytes must be reconciled after the growth-triggered major GC"
    );
}

#[test]
fn external_pressure_keeps_the_ledger_peak_bounded() {
    let account = otter_resource::ResourceAccount::new(otter_resource::ResourceLimits::default());
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    heap.set_external_bytes_account(&account)
        .expect("install ledger");
    let _ = churn_dead_holders(&mut heap, 128);
    let peak = account
        .snapshot()
        .get(otter_resource::ResourceClass::ExternalBytes)
        .peak();
    assert!(
        peak <= MAJOR_GC_FLOOR_BYTES + HOLDER_BYTES,
        "ledger ExternalBytes peak {peak} must follow the major-GC budget"
    );
}
