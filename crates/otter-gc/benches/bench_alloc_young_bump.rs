//! Bench: young-gen bump allocation throughput.
//!
//! Target: ≤ 10 ns per `GcHeap::alloc<Cell>`.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, init_cage_with_size};

struct Cell {
    _payload: u64,
}

impl Traceable for Cell {
    const TYPE_TAG: u8 = 0xF0;
    unsafe fn trace_slots(_: *mut Self, _: &mut SlotVisitor<'_>) {}
}

fn bench(c: &mut Criterion) {
    // Big cage so allocation churn never hits scavenge.
    let _ = init_cage_with_size(512 * 1024 * 1024);
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Cell>();
    let mut group = c.benchmark_group("alloc_young_bump");
    group.throughput(Throughput::Elements(1));
    group.bench_function("Cell", |b| {
        b.iter(|| {
            let _ = heap.alloc(Cell { _payload: 0 });
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
