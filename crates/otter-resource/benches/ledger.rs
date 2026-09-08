//! Bench: steady-state cost of the shared resource ledger.
//!
//! Measures one ledger operation in isolation so a hot path's accounting
//! share can be derived from its call count: single-class reserve/release,
//! lease resize (the GC external-bytes mirror path), multi-class atomic
//! admission (worker and transport messages), rejection at the limit,
//! snapshot capture, and the same resize loop under thread contention on one
//! shared account.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use otter_resource::{ResourceAccount, ResourceClass, ResourceLimits};

fn unlimited() -> ResourceAccount {
    ResourceAccount::new(ResourceLimits::default())
}

fn limited(class: ResourceClass, limit: u64) -> ResourceAccount {
    ResourceAccount::new(ResourceLimits::builder().limit(class, limit).build())
}

fn bench_single_thread(c: &mut Criterion) {
    let mut group = c.benchmark_group("ledger");

    let account = unlimited();
    group.bench_function("reserve_exact_release_4k", |b| {
        b.iter(|| {
            let lease = account
                .reserve_exact(ResourceClass::ExternalBytes, 4096)
                .expect("reserve");
            std::hint::black_box(lease.amount());
            drop(lease);
        });
    });

    let mut lease = account
        .reserve_exact(ResourceClass::ExternalBytes, 1 << 20)
        .expect("lease");
    group.bench_function("resize_grow_shrink_4k", |b| {
        b.iter(|| {
            let base = lease.amount();
            lease.resize(base + 4096).expect("grow");
            lease.resize(base).expect("shrink");
            std::hint::black_box(lease.amount());
        });
    });
    drop(lease);

    let message: [(ResourceClass, u64); 2] = [
        (ResourceClass::QueuedMessages, 1),
        (ResourceClass::QueuedMessageBytes, 4096),
    ];
    group.bench_function("reserve_exact_many_2_release", |b| {
        b.iter(|| {
            let set = account.reserve_exact_many(&message).expect("many");
            std::hint::black_box(set.amount(ResourceClass::QueuedMessages));
            drop(set);
        });
    });

    let family = unlimited();
    group.bench_function("worker_message_two_accounts", |b| {
        b.iter(|| {
            let main = account.reserve_exact_many(&message).expect("main");
            let fam = family.reserve_exact_many(&message).expect("family");
            std::hint::black_box((main.is_empty(), fam.is_empty()));
            drop(fam);
            drop(main);
        });
    });

    let full = limited(ResourceClass::Timers, 0);
    group.bench_function("reject_at_limit", |b| {
        b.iter(|| {
            let result = full.reserve_exact(ResourceClass::Timers, 1);
            std::hint::black_box(result.is_err());
        });
    });

    group.bench_function("snapshot", |b| {
        b.iter(|| std::hint::black_box(account.snapshot()));
    });

    group.finish();
}

/// Each thread owns one lease on the shared account and grows/shrinks it in a
/// loop: the exact traffic that N isolates mirroring external bytes into one
/// family ledger produce. Reports wall time per operation.
fn contended_resize(account: &ResourceAccount, threads: usize, ops_per_thread: u64) -> Duration {
    let barrier = Arc::new(Barrier::new(threads + 1));
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let account = account.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut lease = account
                    .reserve_exact(ResourceClass::ExternalBytes, 1 << 20)
                    .expect("lease");
                barrier.wait();
                for _ in 0..ops_per_thread {
                    let base = lease.amount();
                    lease.resize(base + 4096).expect("grow");
                    lease.resize(base).expect("shrink");
                }
                std::hint::black_box(lease.amount());
            })
        })
        .collect();
    barrier.wait();
    let start = Instant::now();
    for worker in workers {
        worker.join().expect("worker");
    }
    start.elapsed()
}

fn bench_contended(c: &mut Criterion) {
    let mut group = c.benchmark_group("ledger_contended_resize");
    group.sample_size(20);
    let account = unlimited();
    for threads in [1usize, 2, 4, 8] {
        group.bench_with_input(
            BenchmarkId::from_parameter(threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    // Each iteration is one grow+shrink pair on every thread;
                    // amortise thread start-up over a large batch.
                    let ops = iters.max(20_000);
                    let elapsed = contended_resize(&account, threads, ops);
                    elapsed.mul_f64(iters as f64 / ops as f64)
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_single_thread, bench_contended);
criterion_main!(benches);
