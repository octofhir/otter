//! Scavenger: alloc 100 young, hold 50, scavenge, assert 50
//! survived and all reads through the locals see the new
//! offsets.

use otter_gc::raw::RawGc;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, HandleScope};
use std::sync::atomic::{AtomicUsize, Ordering};

static YOUNG_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct Cell {
    payload: u32,
}

impl Traceable for Cell {
    const TYPE_TAG: u8 = 0x40;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

struct DropCell;

impl Traceable for DropCell {
    const TYPE_TAG: u8 = 0x41;
    unsafe fn trace_slots(_this: *mut Self, _v: &mut SlotVisitor<'_>) {}
}

impl Drop for DropCell {
    fn drop(&mut self) {
        YOUNG_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn scavenge_keeps_rooted_objects_alive() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Cell>();

    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let mut roots = Vec::new();
    let mut transient_offsets = Vec::new();
    for i in 0..100u32 {
        let g = heap.alloc(Cell { payload: i }).expect("alloc");
        if i % 2 == 0 {
            roots.push(scope.local(g));
        } else {
            transient_offsets.push(g.offset());
        }
    }
    // Trigger scavenge.
    heap.collect_minor(otter_gc::EmptyRoots).expect("minor GC");

    // Each rooted local still resolves to a valid header whose
    // payload matches the original.
    for (idx, l) in roots.iter().enumerate() {
        let g = l.get();
        assert!(!g.is_null(), "local {idx} went to null after scavenge");
        // SAFETY: STW is over, but we have not allocated since;
        // the local's offset is fresh.
        let header_ptr = g.as_header_ptr();
        let payload_ptr = unsafe {
            (header_ptr as *mut u8).add(std::mem::size_of::<otter_gc::GcHeader>()) as *mut Cell
        };
        let v = unsafe { (*payload_ptr).payload };
        assert_eq!(v, (idx as u32) * 2, "payload mismatch after scavenge");
    }
}

#[test]
fn scavenge_with_external_root_survives() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Cell>();
    let g = heap.alloc(Cell { payload: 42 }).unwrap();
    let mut slot = g.raw();
    // Run scavenge feeding `slot` as an external root.
    heap.collect_minor_with_roots(&mut |v| v(&mut slot as *mut RawGc))
        .expect("minor GC");
    // Slot got rewritten to the new offset.
    assert!(!slot.is_null());
    let new_g: otter_gc::Gc<Cell> = unsafe { slot.cast() };
    let header = new_g.as_header_ptr();
    let payload =
        unsafe { (header as *mut u8).add(std::mem::size_of::<otter_gc::GcHeader>()) as *mut Cell };
    assert_eq!(unsafe { (*payload).payload }, 42);
}

#[test]
fn scavenge_drops_unreachable_young_bodies() {
    YOUNG_DROP_COUNT.store(0, Ordering::SeqCst);
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<DropCell>();

    let _dead = heap.alloc(DropCell).expect("alloc young body");
    heap.collect_minor(otter_gc::EmptyRoots).expect("minor GC");

    assert_eq!(YOUNG_DROP_COUNT.load(Ordering::SeqCst), 1);
}
