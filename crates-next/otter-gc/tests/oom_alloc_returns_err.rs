//! Cap-driven OOM: a single allocation larger than the
//! configured per-heap cap is refused, surfaced as
//! [`OutOfMemory::HeapCapExceeded`], and the slot is **not**
//! materialised. Architecture plan §2.1 caveat — task 73.
//!
//! # See also
//!
//! - <https://tc39.es/ecma262/#sec-host-make-job-callback> (host
//!   ergonomics; OOM is a host-level signal, not a spec
//!   algorithm).

use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, OutOfMemory};

/// 4 KiB payload — comfortably above the 1 KiB cap below and
/// below the LOS threshold so the cap rejection path runs in
/// new-space.
struct Big {
    _payload: [u8; 4 * 1024],
}

impl Traceable for Big {
    const TYPE_TAG: u8 = 0x30;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

#[test]
fn alloc_over_cap_returns_heap_cap_exceeded() {
    let mut heap = GcHeap::with_max_heap_bytes(1024).expect("heap");
    let err = heap
        .alloc(Big {
            _payload: [0; 4 * 1024],
        })
        .expect_err("alloc must be refused under 1 KiB cap");
    match err {
        OutOfMemory::HeapCapExceeded {
            requested_bytes,
            heap_limit_bytes,
        } => {
            assert!(
                requested_bytes >= 4 * 1024,
                "requested_bytes covers payload + header, got {requested_bytes}"
            );
            assert_eq!(heap_limit_bytes, 1024);
        }
        other => panic!("expected HeapCapExceeded, got {other:?}"),
    }
    // Cap rejection sets the cooperative cancellation flag too.
    assert!(
        heap.oom_flag().load(std::sync::atomic::Ordering::Relaxed),
        "oom_flag must be set after a cap rejection"
    );
    // tracked_bytes was rolled back — the failed alloc must not
    // leave phantom accounting behind.
    assert_eq!(heap.tracked_bytes(), 0);
    assert_eq!(heap.max_heap_bytes(), 1024);
}
