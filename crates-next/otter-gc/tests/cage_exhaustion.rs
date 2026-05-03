//! Cage exhaustion → `OutOfMemory::CageExhausted`. Allocates a
//! small cage (8 MiB = 32 pages) and bumps allocations until the
//! scavenger can no longer carve a fresh new-space page.
//!
//! See task 72 — pointer-compression invariants (NF9).

use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, OutOfMemory, init_cage_with_size};

const SMALL_CAGE: usize = 16 * 1024 * 1024; // 64 pages

// Use a payload bigger than LARGE_OBJECT_THRESHOLD so each Big
// consumes its own page; cage exhaustion is then trivially
// observable.
struct Big {
    _payload: [u8; 200 * 1024], // 200 KiB — > 1/2 page.
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
        match heap.alloc(Big {
            _payload: [0; 200 * 1024],
        }) {
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
