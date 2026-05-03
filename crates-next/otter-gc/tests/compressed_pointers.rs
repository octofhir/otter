//! Pointer-compression invariants: `Gc<T>` is a `u32`,
//! decompresses identically every time, and stays inside the
//! cage for every allocation produced by the heap.
//!
//! See task 72 — gates: every allocation lands inside the cage
//! and the offset round-trips identity.

use std::mem::size_of;

use otter_gc::compressed::cage_base_addr;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, RawGc, cage_size};

struct Bytes32 {
    #[allow(dead_code)]
    data: [u8; 32],
}

impl Traceable for Bytes32 {
    const TYPE_TAG: u8 = 0x10;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

#[test]
fn gc_is_u32_sized() {
    assert_eq!(size_of::<otter_gc::Gc<Bytes32>>(), 4);
    assert_eq!(size_of::<RawGc>(), 4);
}

#[test]
fn alloc_offsets_round_trip_inside_cage() {
    let mut heap = GcHeap::new().expect("heap");
    let mut offsets = Vec::new();
    for i in 0..256 {
        let g = heap
            .alloc(Bytes32 {
                data: [i as u8; 32],
            })
            .expect("alloc");
        let off = g.offset();
        let cage_lo = cage_base_addr();
        let cage_hi = cage_lo + cage_size();
        let addr = g.as_header_ptr() as usize;
        assert!(
            addr >= cage_lo && addr < cage_hi,
            "Gc<T> outside cage: addr=0x{:x} cage=[0x{:x}, 0x{:x})",
            addr,
            cage_lo,
            cage_hi
        );
        // Decompression identity.
        assert_eq!(addr - cage_lo, off as usize);
        offsets.push(off);
    }
    // Every offset is unique (no aliasing).
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), offsets.len(), "duplicate offsets observed");
}
