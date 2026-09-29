//! Cage exhaustion → `OutOfMemory::CageExhausted`. Allocates a
//! small cage (8 MiB = 32 pages) and bumps allocations until the
//! scavenger can no longer carve a fresh new-space page.
//!
//! See task 72 — pointer-compression invariants (NF9).

use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, OutOfMemory, init_cage_with_size};

const SMALL_CAGE: usize = 16 * 1024 * 1024; // 64 pages

// Each cell carries a trailing payload bigger than
// LARGE_OBJECT_THRESHOLD so it consumes its own page; cage exhaustion is
// then trivially observable. The payload trails the body instead of living
// in it, so no 200 KiB value is moved through the allocator by value.
const BIG_TRAILING_BYTES: usize = 200 * 1024; // > 1/2 page.

struct Big {
    _length: u64,
}

impl Traceable for Big {
    const TYPE_TAG: u8 = 0x20;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

#[test]
fn cage_exhaustion_surfaces_out_of_memory() {
    // Use a small cage so we can actually exhaust it.
    let _ = init_cage_with_size(SMALL_CAGE);
    let mut heap = GcHeap::new().expect("heap");
    let mut allocations = 0;
    loop {
        match heap.alloc_trailing_with_roots(Big { _length: 0 }, BIG_TRAILING_BYTES, &mut |_| {}) {
            Ok(_) => allocations += 1,
            Err(e) => {
                assert!(matches!(e, OutOfMemory::CageExhausted));
                break;
            }
        }
        if allocations > 10_000 {
            panic!("cage failed to exhaust after {allocations} allocations");
        }
    }
    assert!(allocations > 0, "exhausted with no successful allocations");
}
