//! Bench: handle-scope creation and `Local` rooting overhead.
//!
//! Targets the RAII rooting path used by VM/native code around allocation and
//! calls.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, HandleScope, init_cage_with_size};

struct Cell;

impl Traceable for Cell {
    const TYPE_TAG: u8 = 0xF5;
    unsafe fn trace_slots(_: *mut Self, _: &mut SlotVisitor<'_>) {}
}

fn bench(c: &mut Criterion) {
    let _ = init_cage_with_size(512 * 1024 * 1024);
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Cell>();
    let cell = heap.alloc(Cell).expect("alloc");

    let mut group = c.benchmark_group("handle_scope_overhead");
    group.throughput(Throughput::Elements(16));
    group.bench_function("scope_with_16_locals", |b| {
        b.iter(|| {
            let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
            for _ in 0..16 {
                let local = scope.local(cell);
                std::hint::black_box(local.get());
            }
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
