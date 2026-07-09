//! Promotion reservation must fail before a copying collection mutates roots.
//!
//! # Invariants
//! - cage exhaustion leaves root offsets and forwarding headers unchanged;
//! - a partially acquired reservation is returned to the cage;
//! - old space receives no partial reservation pages.

use otter_gc::header::HEADER_SIZE;
use otter_gc::raw::{RawGc, SlotVisitor, TraceTable};
use otter_gc::scavenger::scavenge;
use otter_gc::space::{NewSpace, OldSpace, align_alloc_size};
use otter_gc::{
    GcHeader, OutOfMemory, PAGE_SIZE, Traceable, cage_base, cage_stats, init_cage_with_size,
};

struct Cell;

impl Traceable for Cell {
    const TYPE_TAG: u8 = 0xE5;

    unsafe fn trace_slots(_this: *mut Self, _visitor: &mut SlotVisitor<'_>) {}
}

unsafe fn initialize_cell(offset: u32, young: bool, aligned: usize) {
    // SAFETY: callers pass a fresh allocation with room for header + Cell.
    unsafe {
        let header = cage_base().add(offset as usize).cast::<GcHeader>();
        let value = if young {
            GcHeader::new_young(Cell::TYPE_TAG, aligned as u32)
        } else {
            GcHeader::new(Cell::TYPE_TAG, aligned as u32)
        };
        std::ptr::write(header, value);
        std::ptr::write(
            cage_base()
                .add(offset as usize + HEADER_SIZE)
                .cast::<Cell>(),
            Cell,
        );
    }
}

#[test]
fn promotion_preflight_oom_leaves_heap_unmodified() {
    // Page 0 is reserved. NewSpace consumes two pages and the old parent one,
    // leaving no page for the two-page promotion reservation.
    init_cage_with_size(PAGE_SIZE * 4).expect("small cage");
    let mut new_space = NewSpace::new(1).expect("new space");
    let mut old_space = OldSpace::new();
    let mut trace_table = TraceTable::new();
    trace_table.register::<Cell>();

    let aligned = align_alloc_size(HEADER_SIZE + std::mem::size_of::<Cell>());
    let young_offset = new_space.alloc(aligned).expect("young cell");
    let old_offset = old_space.alloc(aligned).expect("old parent");
    unsafe {
        initialize_cell(young_offset, true, aligned);
        initialize_cell(old_offset, false, aligned);
    }

    let free_before = cage_stats().expect("cage stats").free_pages;
    let old_pages_before = old_space.page_count();
    let mut root = RawGc(young_offset);
    let mut remembered = vec![RawGc(old_offset)];

    let result = unsafe {
        scavenge(
            &mut new_space,
            &mut old_space,
            &trace_table,
            &[&mut root as *mut RawGc],
            &mut |_| {},
            &[],
            &[],
            &mut remembered,
        )
    };

    assert_eq!(result, Err(OutOfMemory::CageExhausted));
    assert_eq!(root, RawGc(young_offset));
    let young_header = unsafe { cage_base().add(young_offset as usize).cast::<GcHeader>() };
    assert!(!unsafe { (*young_header).is_forwarded() });
    assert_eq!(old_space.page_count(), old_pages_before);
    assert_eq!(cage_stats().expect("cage stats").free_pages, free_before);
}
