//! Microbenchmark for `JsString::index_of` / `starts_with`.
//!
//! Compares the SWAR-assisted hot path (added with ROADMAP P8)
//! against the previous behaviour by routing the same inputs
//! through both `crate::swar::find_byte` (the new path) and a
//! scalar reference loop. Three workloads:
//!
//! - `index_of_late_match_1KiB`  : 1024-byte clean haystack, 5-byte
//!   needle near the tail — exercises the Latin-1 SWAR path.
//! - `index_of_no_match_4KiB`    : 4 KiB haystack with no matches —
//!   worst case for scalar, best case for SWAR.
//! - `starts_with_short`         : 32-byte haystack, 8-byte prefix —
//!   verifies no regression on tiny inputs.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use otter_vm::string::JsString;
use otter_vm::swar::{find_byte, rfind_byte};

fn make_clean(len: usize, marker_at: Option<usize>) -> Vec<u8> {
    let mut buf: Vec<u8> = (0..len).map(|i| b'a' + (i as u8 % 25)).collect();
    if let Some(idx) = marker_at {
        buf[idx..idx + 5].copy_from_slice(b"NEEDL");
    }
    buf
}

fn scalar_find_byte(bytes: &[u8], c: u8, from: usize) -> Option<usize> {
    bytes[from..].iter().position(|&b| b == c).map(|p| p + from)
}

fn bench_string_search(c: &mut Criterion) {
    let mut heap = otter_gc::GcHeap::new().expect("gc heap");

    let buf_late = make_clean(1024, Some(1000));
    let needle_5 = b"NEEDL";

    let mut g = c.benchmark_group("string_search/index_of_late_match_1KiB");
    g.throughput(Throughput::Bytes(buf_late.len() as u64));
    g.bench_function("scalar", |b| {
        b.iter(|| {
            // Naïve byte-at-a-time scan with the same verify shape.
            let h = black_box(&buf_late);
            let n = black_box(&needle_5[..]);
            let first = n[0];
            let n_len = n.len();
            let last_start = h.len() - n_len;
            let mut start = 0usize;
            let mut found = None;
            while start <= last_start {
                let Some(rel) = scalar_find_byte(&h[start..=last_start], first, 0) else {
                    break;
                };
                let i = start + rel;
                if h[i..i + n_len] == *n {
                    found = Some(i);
                    break;
                }
                start = i + 1;
            }
            found
        })
    });
    g.bench_function("swar", |b| {
        b.iter(|| {
            let h = black_box(&buf_late);
            let n = black_box(&needle_5[..]);
            let first = n[0];
            let n_len = n.len();
            let last_start = h.len() - n_len;
            let mut start = 0usize;
            let mut found = None;
            while start <= last_start {
                let Some(rel) = find_byte(&h[start..=last_start], first, 0) else {
                    break;
                };
                let i = start + rel;
                if h[i..i + n_len] == *n {
                    found = Some(i);
                    break;
                }
                start = i + 1;
            }
            found
        })
    });
    g.finish();

    let buf_4k = make_clean(4096, None);
    let mut g = c.benchmark_group("string_search/index_of_no_match_4KiB");
    g.throughput(Throughput::Bytes(buf_4k.len() as u64));
    g.bench_function("scalar", |b| {
        b.iter(|| scalar_find_byte(black_box(&buf_4k), black_box(b'?'), 0))
    });
    g.bench_function("swar", |b| {
        b.iter(|| find_byte(black_box(&buf_4k), black_box(b'?'), 0))
    });
    g.finish();

    let buf_4k = make_clean(4096, None);
    let mut g = c.benchmark_group("string_search/last_index_of_4KiB");
    g.throughput(Throughput::Bytes(buf_4k.len() as u64));
    g.bench_function("scalar_rfind", |b| {
        b.iter(|| {
            black_box(&buf_4k)
                .iter()
                .rposition(|&x| x == black_box(b'?'))
        })
    });
    g.bench_function("swar_rfind", |b| {
        b.iter(|| rfind_byte(black_box(&buf_4k), black_box(b'?')))
    });
    g.finish();

    // End-to-end through JsString::starts_with so we measure the
    // full Latin-1 fast path (including `as_latin1`).
    let h_str = JsString::from_latin1(&make_clean(32, Some(0)), &mut heap).unwrap();
    let p_str = JsString::from_latin1(b"NEEDLabcd", &mut heap).unwrap();
    let mut g = c.benchmark_group("string_search/starts_with_short");
    g.bench_function("starts_with_latin1_path", |b| {
        b.iter(|| black_box(h_str).starts_with(black_box(p_str), 0, &heap))
    });
    g.finish();
}

criterion_group!(benches, bench_string_search);
criterion_main!(benches);
