use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use otter_vm_core::promise::{JsPromiseJob, JsPromiseJobKind};
use otter_vm_core::value::Value;
use otter_vm_runtime::event_loop::EventLoop;
use otter_vm_runtime::microtask::{JsJobQueue, MicrotaskQueue, NextTickQueue};
use std::hint::black_box;

fn sample_js_job() -> JsPromiseJob {
    JsPromiseJob {
        kind: JsPromiseJobKind::Fulfill,
        callback: Value::undefined(),
        this_arg: Value::undefined(),
        result_promise: None,
    }
}

fn bench_js_job_queue(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_js_job_queue");
    let n = 20_000usize;

    group.bench_function("single_thread_enqueue_dequeue", |b| {
        b.iter_batched(
            JsJobQueue::new,
            |q| {
                for _ in 0..n {
                    q.enqueue(sample_js_job(), Vec::new());
                }
                let mut drained = 0usize;
                while q.dequeue().is_some() {
                    drained += 1;
                }
                black_box(drained);
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("mpsc_4_producers_enqueue_then_drain", |b| {
        b.iter(|| {
            let q = Arc::new(JsJobQueue::new());
            let per_producer = n / 4;
            let mut threads = Vec::with_capacity(4);

            for _ in 0..4 {
                let q = Arc::clone(&q);
                threads.push(thread::spawn(move || {
                    for _ in 0..per_producer {
                        q.enqueue(sample_js_job(), Vec::new());
                    }
                }));
            }

            for t in threads {
                t.join().expect("producer thread failed");
            }

            let mut drained = 0usize;
            while q.dequeue().is_some() {
                drained += 1;
            }
            black_box(drained);
        });
    });

    group.finish();
}

fn bench_microtask_queue(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_microtask_queue");
    let n = 20_000usize;

    group.bench_function("single_thread_enqueue_dequeue", |b| {
        b.iter_batched(
            MicrotaskQueue::new,
            |q| {
                for _ in 0..n {
                    q.enqueue(|| {});
                }
                let mut drained = 0usize;
                while q.dequeue().is_some() {
                    drained += 1;
                }
                black_box(drained);
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_next_tick_queue(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_next_tick_queue");
    let n = 20_000usize;

    group.bench_function("dequeue_loop", |b| {
        b.iter_batched(
            || {
                let q = NextTickQueue::new();
                for _ in 0..n {
                    q.enqueue(Value::undefined(), Vec::new());
                }
                q
            },
            |q| {
                let mut drained = 0usize;
                while q.dequeue().is_some() {
                    drained += 1;
                }
                black_box(drained);
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("dequeue_batch_64", |b| {
        b.iter_batched(
            || {
                let q = NextTickQueue::new();
                for _ in 0..n {
                    q.enqueue(Value::undefined(), Vec::new());
                }
                q
            },
            |q| {
                let mut drained = 0usize;
                let mut batch = Vec::with_capacity(64);
                loop {
                    batch.clear();
                    let count = q.dequeue_batch(64, &mut batch);
                    if count == 0 {
                        break;
                    }
                    drained += count;
                }
                black_box(drained);
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_event_loop_immediates(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_event_loop");
    let n = 20_000usize;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("tokio runtime");

    group.bench_function("run_20k_immediates", |b| {
        b.iter_batched(
            || {
                let event_loop = EventLoop::new();
                let counter = Arc::new(AtomicUsize::new(0));
                for _ in 0..n {
                    let counter = Arc::clone(&counter);
                    event_loop.schedule_immediate(
                        move || {
                            counter.fetch_add(1, Ordering::Relaxed);
                        },
                        true,
                    );
                }
                (event_loop, counter)
            },
            |(event_loop, counter)| {
                rt.block_on(event_loop.run_until_complete_async());
                black_box(counter.load(Ordering::Relaxed));
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(
    runtime_async_queues,
    bench_js_job_queue,
    bench_microtask_queue,
    bench_next_tick_queue,
    bench_event_loop_immediates
);
criterion_main!(runtime_async_queues);
