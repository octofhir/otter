//! Miri smoke: a tiny allocate-trace-collect program that runs
//! cleanly under `cargo +nightly miri test`. Intentionally
//! small to keep miri runtime bounded; the full set of
//! integration tests exercises the rest under miri too.

use otter_gc::raw::RawGc;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{Gc, GcHeap, HandleScope};

struct Cell {
    next: Gc<Cell>,
}
impl Traceable for Cell {
    const TYPE_TAG: u8 = 0xB0;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        unsafe {
            let slot = std::ptr::addr_of_mut!((*this).next) as *mut RawGc;
            v(slot);
        }
    }
}

#[test]
fn miri_smoke_alloc_collect() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Cell>();

    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let a = heap.alloc(Cell { next: Gc::null() }).unwrap();
    let b = heap.alloc(Cell { next: a }).unwrap();
    let _l = scope.local(b);
    heap.collect_minor(otter_gc::EmptyRoots).expect("minor GC");
    heap.collect_full(&mut |_| {}).expect("full GC");
}
