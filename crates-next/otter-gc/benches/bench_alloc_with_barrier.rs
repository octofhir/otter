//! Bench: alloc + one pointer-store via the write barrier.
//!
//! Target: ≤ 30 ns per iteration.

use criterion::{Criterion, criterion_group, criterion_main};
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{Gc, GcHeap, RawGc, init_cage_with_size};

struct Pair {
    next: Gc<Pair>,
}

impl Traceable for Pair {
    const TYPE_TAG: u8 = 0xF1;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        unsafe {
            v(std::ptr::addr_of_mut!((*this).next) as *mut RawGc);
        }
    }
}

fn bench(c: &mut Criterion) {
    let _ = init_cage_with_size(512 * 1024 * 1024);
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Pair>();
    c.bench_function("alloc_with_barrier", |b| {
        b.iter(|| {
            let p = heap.alloc(Pair { next: Gc::null() }).expect("alloc");
            let q = heap.alloc(Pair { next: Gc::null() }).expect("alloc");
            // Write q into p.next + barrier.
            unsafe {
                let payload = (p.as_header_ptr() as *mut u8)
                    .add(std::mem::size_of::<otter_gc::GcHeader>())
                    as *mut Pair;
                let slot = std::ptr::addr_of_mut!((*payload).next);
                (*slot) = q;
                heap.write_barrier(p, slot, q);
            }
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
