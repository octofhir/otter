//! Emergency-collect retry path: under a tight cap, an
//! allocation that would overshoot triggers one full GC, and if
//! the GC reclaims enough room the retry succeeds. Covers the
//! "retry-once-then-fail" contract from task 73 step 3.
//!
//! # See also
//!
//! - GC architecture plan §2.1, §7.5.

use otter_gc::handle::HandleScope;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, Local};

/// 6 KiB payload — two of these (~12 KiB) overshoot an 8 KiB
/// cap, but each individual one fits.
struct Block {
    _payload: [u8; 6 * 1024],
}

impl Traceable for Block {
    const TYPE_TAG: u8 = 0x31;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

#[test]
fn emergency_collect_reclaims_unrooted_then_retry_succeeds() {
    let mut heap = GcHeap::with_max_heap_bytes(8 * 1024).expect("heap");

    // First allocation. Wrap a HandleScope explicitly so we can
    // model the "drop the only handle" step from the task spec.
    let scope_ptr = heap.handle_stack_ptr();
    {
        // SAFETY: scope_ptr points at the heap's stable
        // Box<HandleStack>; the heap outlives the scope.
        let scope = unsafe { HandleScope::from_ptr(scope_ptr) };
        let first = heap
            .alloc(Block {
                _payload: [0; 6 * 1024],
            })
            .expect("first 6 KiB alloc fits the 8 KiB cap");
        let _rooted: Local<'_, Block> = scope.local(first);
        // Scope drops here, taking the rooted handle with it.
        drop(scope);
    }

    // Sanity: after the first alloc, tracked_bytes is in the
    // 6 KiB ballpark.
    let before = heap.tracked_bytes();
    assert!(
        (6 * 1024..=8 * 1024).contains(&before),
        "first alloc booked ~6 KiB, got {before}"
    );

    // Second alloc would project to ~12 KiB > 8 KiB cap;
    // emergency full GC runs, the unrooted first block is
    // reclaimed, retry succeeds.
    let second = heap.alloc(Block {
        _payload: [0; 6 * 1024],
    });
    assert!(
        second.is_ok(),
        "second alloc must succeed after emergency collect, got {second:?}"
    );
    // tracked_bytes after the retry should be back in the
    // 6 KiB-only ballpark (the first block was reclaimed).
    let after = heap.tracked_bytes();
    assert!(
        after <= 8 * 1024,
        "tracked_bytes must respect cap after retry, got {after}"
    );
}
