//! Bench: full 4 MiB young-gen scavenge with ~50 % survival.
//!
//! Target: ≤ 5 ms wall.

use criterion::{Criterion, criterion_group, criterion_main};
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, HandleScope, init_cage_with_size};

struct Cell {
    _payload: [u8; 64],
}

impl Traceable for Cell {
    const TYPE_TAG: u8 = 0xF2;
    unsafe fn trace_slots(_: *mut Self, _: &mut SlotVisitor<'_>) {}
}

fn bench(c: &mut Criterion) {
    let _ = init_cage_with_size(512 * 1024 * 1024);
    c.bench_function("scavenge_4mb_50pct", |b| {
        b.iter_custom(|iters| {
            let start = std::time::Instant::now();
            for _ in 0..iters {
                let mut heap = GcHeap::new().expect("heap");
                heap.register_traceable::<Cell>();
                let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
                // Allocate ~4 MiB of 72-byte (cell-aligned) Cells
                // = ~58_000 cells. Root every other one for ~50%
                // survival.
                let mut roots = Vec::with_capacity(30_000);
                for i in 0..58_000u32 {
                    let g = match heap.alloc(Cell { _payload: [0; 64] }) {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    if i & 1 == 0 {
                        roots.push(scope.local(g));
                    }
                }
                heap.collect_minor(otter_gc::EmptyRoots);
            }
            start.elapsed()
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
