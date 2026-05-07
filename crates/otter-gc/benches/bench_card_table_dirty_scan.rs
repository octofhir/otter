//! Bench: card-table dirty-card scan over one old-space page.
//!
//! This isolates remembered-set scan overhead from object tracing.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use otter_gc::{CARD_SIZE, PAGE_SIZE, Page, SpaceKind, init_cage_with_size};

fn bench(c: &mut Criterion) {
    let _ = init_cage_with_size(512 * 1024 * 1024);
    let page = Page::new(SpaceKind::Old).expect("page");
    for offset in (0..PAGE_SIZE).step_by(CARD_SIZE * 8) {
        page.mark_card(offset);
    }

    let mut group = c.benchmark_group("card_table_dirty_scan");
    group.throughput(Throughput::Elements((PAGE_SIZE / (CARD_SIZE * 8)) as u64));
    group.bench_function("one_page_sparse_dirty", |b| {
        b.iter(|| {
            let mut checksum = 0usize;
            page.header().for_each_dirty_card(|card, offset| {
                checksum = checksum.wrapping_add(card).wrapping_add(offset);
            });
            std::hint::black_box(checksum);
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
