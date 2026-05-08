//! Microbenchmark for the JSON string-literal scanner.
//!
//! Compares the scalar byte-at-a-time loop against the SWAR
//! 8-bytes-per-iteration scanner used by `JSON.stringify` and
//! `JSON.parse`. Three workloads:
//! - `clean_long`  : 1024-byte ASCII payload, no escapes — best case.
//! - `clean_short` : 32-byte ASCII payload — verifies no regression
//!   for sub-chunk inputs that never enter the SWAR loop.
//! - `escape_dense`: 256-byte payload with an escape every 16 bytes
//!   — short clean spans, frequent SWAR-loop early exits.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use otter_vm::json::scan::{find_first_escape_pub, find_first_escape_scalar};

fn make_clean(len: usize) -> Vec<u8> {
    (0..len).map(|i| b'a' + (i as u8 % 26)).collect()
}

fn make_dense_escapes(len: usize, period: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            if i % period == period - 1 {
                b'"'
            } else {
                b'a' + (i as u8 % 26)
            }
        })
        .collect()
}

/// Run the scanner repeatedly across the entire buffer, simulating
/// the inner loop of `write_string_literal`. We restart the scan
/// after every hit so escape-dense inputs don't degenerate to a
/// single call.
fn run_scan_full<F: Fn(&[u8], usize) -> usize>(buf: &[u8], scan: F) -> u64 {
    let mut acc: u64 = 0;
    let mut i = 0;
    while i < buf.len() {
        let next = scan(buf, i);
        acc = acc.wrapping_add(next as u64);
        if next >= buf.len() {
            break;
        }
        i = next + 1;
    }
    acc
}

fn bench_scan(c: &mut Criterion) {
    let clean_long = make_clean(1024);
    let clean_short = make_clean(32);
    let dense = make_dense_escapes(256, 16);

    let mut g = c.benchmark_group("json_scan/clean_long_1024");
    g.throughput(Throughput::Bytes(clean_long.len() as u64));
    g.bench_function("scalar", |b| {
        b.iter(|| run_scan_full(black_box(&clean_long), find_first_escape_scalar))
    });
    g.bench_function("swar", |b| {
        b.iter(|| run_scan_full(black_box(&clean_long), find_first_escape_pub))
    });
    g.finish();

    let mut g = c.benchmark_group("json_scan/clean_short_32");
    g.throughput(Throughput::Bytes(clean_short.len() as u64));
    g.bench_function("scalar", |b| {
        b.iter(|| run_scan_full(black_box(&clean_short), find_first_escape_scalar))
    });
    g.bench_function("swar", |b| {
        b.iter(|| run_scan_full(black_box(&clean_short), find_first_escape_pub))
    });
    g.finish();

    let mut g = c.benchmark_group("json_scan/escape_dense_256");
    g.throughput(Throughput::Bytes(dense.len() as u64));
    g.bench_function("scalar", |b| {
        b.iter(|| run_scan_full(black_box(&dense), find_first_escape_scalar))
    });
    g.bench_function("swar", |b| {
        b.iter(|| run_scan_full(black_box(&dense), find_first_escape_pub))
    });
    g.finish();
}

criterion_group!(benches, bench_scan);
criterion_main!(benches);
