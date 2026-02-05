//! GC Performance Benchmarks
//!
//! Measures GC pause times and allocation throughput.
//!
//! Run with: `cargo bench -p otter-vm-core gc`
//!
//! Target: GC pause < 10ms for 1MB heap

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::value::Value;
use otter_vm_core::VmContext;
use std::hint::black_box;
use std::sync::Arc;

/// Create a test VM context
fn create_test_context() -> (VmContext, Arc<MemoryManager>) {
    let mm = Arc::new(MemoryManager::new(100_000_000)); // 100MB
    let global = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let ctx = VmContext::new(global, mm.clone());
    (ctx, mm)
}

/// Benchmark GC pause time for various heap sizes
fn gc_pause_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("gc_pause");

    // Test different heap sizes
    for num_objects in [100, 1000, 5000, 10000].iter() {
        group.bench_with_input(
            BenchmarkId::new("objects", num_objects),
            num_objects,
            |b, &n| {
                b.iter_custom(|iters| {
                    let mut total_duration = std::time::Duration::ZERO;

                    for _ in 0..iters {
                        let (ctx, mm) = create_test_context();

                        // Pre-allocate objects to simulate a working heap
                        // Keep some alive, let others become garbage
                        let mut live_objects = Vec::with_capacity(n / 2);
                        for i in 0..n {
                            let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                            obj.set(PropertyKey::string("value"), Value::int32(i as i32));
                            if i % 2 == 0 {
                                live_objects.push(obj);
                            }
                            // Odd-indexed objects become garbage
                        }

                        // Measure GC pause time
                        let start = std::time::Instant::now();
                        ctx.collect_garbage();
                        total_duration += start.elapsed();

                        // Prevent optimization from eliminating live_objects
                        black_box(&live_objects);
                    }

                    total_duration
                });
            },
        );
    }

    group.finish();
}

/// Benchmark allocation throughput (objects per second)
fn allocation_throughput_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("allocation_throughput");

    group.bench_function("simple_objects_1000", |b| {
        b.iter(|| {
            let (_, mm) = create_test_context();
            for i in 0..1000 {
                let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                obj.set(PropertyKey::string("x"), Value::int32(i));
                black_box(&obj);
            }
        });
    });

    group.bench_function("objects_with_properties_1000", |b| {
        b.iter(|| {
            let (_, mm) = create_test_context();
            for i in 0..1000 {
                let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                // Add multiple properties
                obj.set(PropertyKey::string("a"), Value::int32(i));
                obj.set(PropertyKey::string("b"), Value::int32(i * 2));
                obj.set(PropertyKey::string("c"), Value::int32(i * 3));
                black_box(&obj);
            }
        });
    });

    group.finish();
}

/// Benchmark GC with circular references
fn gc_circular_refs_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("gc_circular");

    group.bench_function("cycles_100", |b| {
        b.iter_custom(|iters| {
            let mut total_duration = std::time::Duration::ZERO;

            for _ in 0..iters {
                let (ctx, mm) = create_test_context();

                // Create 100 circular reference pairs
                for _ in 0..100 {
                    let a = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                    let b = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                    a.set(PropertyKey::string("ref"), Value::object(b));
                    b.set(PropertyKey::string("ref"), Value::object(a));
                    // Drop both - creates garbage cycle
                }

                let start = std::time::Instant::now();
                ctx.collect_garbage();
                total_duration += start.elapsed();
            }

            total_duration
        });
    });

    group.bench_function("self_refs_100", |b| {
        b.iter_custom(|iters| {
            let mut total_duration = std::time::Duration::ZERO;

            for _ in 0..iters {
                let (ctx, mm) = create_test_context();

                // Create 100 self-referencing objects
                for _ in 0..100 {
                    let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                    obj.set(PropertyKey::string("self"), Value::object(obj));
                }

                let start = std::time::Instant::now();
                ctx.collect_garbage();
                total_duration += start.elapsed();
            }

            total_duration
        });
    });

    group.finish();
}

/// Benchmark mark phase performance (tracing)
fn gc_mark_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("gc_mark");

    // Benchmark with deep object graphs
    group.bench_function("deep_chain_100", |b| {
        b.iter_custom(|iters| {
            let mut total_duration = std::time::Duration::ZERO;

            for _ in 0..iters {
                let (ctx, mm) = create_test_context();

                // Create a chain of 100 objects
                let root = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                let mut current = root;
                for depth in 0..100 {
                    let next = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                    current.set(PropertyKey::string("next"), Value::object(next));
                    current.set(PropertyKey::string("depth"), Value::int32(depth));
                    current = next;
                }

                // Keep root alive
                black_box(&root);

                let start = std::time::Instant::now();
                ctx.collect_garbage();
                total_duration += start.elapsed();

                black_box(&current);
            }

            total_duration
        });
    });

    // Benchmark with wide object graphs (many children per node)
    group.bench_function("wide_tree_depth3_fanout10", |b| {
        b.iter_custom(|iters| {
            let mut total_duration = std::time::Duration::ZERO;

            for _ in 0..iters {
                let (ctx, mm) = create_test_context();

                // Create a tree with fanout 10 and depth 3 (1 + 10 + 100 + 1000 = 1111 objects)
                fn create_tree(mm: &Arc<MemoryManager>, depth: i32) -> GcRef<JsObject> {
                    let node = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                    node.set(PropertyKey::string("depth"), Value::int32(depth));

                    if depth > 0 {
                        for i in 0..10 {
                            let child = create_tree(mm, depth - 1);
                            let key = PropertyKey::string(&format!("child{}", i));
                            node.set(key, Value::object(child));
                        }
                    }

                    node
                }

                let root = create_tree(&mm, 3);
                black_box(&root);

                let start = std::time::Instant::now();
                ctx.collect_garbage();
                total_duration += start.elapsed();
            }

            total_duration
        });
    });

    group.finish();
}

/// Benchmark full GC cycle under realistic workload
fn gc_realistic_workload_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("gc_realistic");

    group.bench_function("mixed_workload", |b| {
        b.iter_custom(|iters| {
            let mut total_duration = std::time::Duration::ZERO;

            for _ in 0..iters {
                let (ctx, mm) = create_test_context();

                // Simulate a realistic workload:
                // - Some long-lived objects
                // - Some short-lived garbage
                // - Some circular references

                // Long-lived objects
                let mut long_lived = Vec::new();
                for i in 0..50 {
                    let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                    obj.set(PropertyKey::string("id"), Value::int32(i));
                    long_lived.push(obj);
                }

                // Short-lived garbage
                for i in 0..200 {
                    let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                    obj.set(PropertyKey::string("temp"), Value::int32(i));
                    // Immediately becomes garbage
                }

                // Some circular references
                for _ in 0..20 {
                    let a = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                    let b = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                    a.set(PropertyKey::string("peer"), Value::object(b));
                    b.set(PropertyKey::string("peer"), Value::object(a));
                }

                black_box(&long_lived);

                let start = std::time::Instant::now();
                ctx.collect_garbage();
                total_duration += start.elapsed();
            }

            total_duration
        });
    });

    group.finish();
}

/// Benchmark memory reclamation
fn gc_reclamation_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("gc_reclamation");

    group.bench_function("reclaim_5000_objects", |b| {
        b.iter(|| {
            let (ctx, mm) = create_test_context();

            // Allocate and immediately drop 5000 objects
            for i in 0..5000 {
                let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                obj.set(PropertyKey::string("data"), Value::int32(i));
            }

            // Measure reclamation
            let reclaimed = ctx.collect_garbage();
            black_box(reclaimed)
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    gc_pause_benchmark,
    allocation_throughput_benchmark,
    gc_circular_refs_benchmark,
    gc_mark_benchmark,
    gc_realistic_workload_benchmark,
    gc_reclamation_benchmark,
);

criterion_main!(benches);
