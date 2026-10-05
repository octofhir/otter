//! Real promotion refusal before forwarding, with a legal smaller prefix.
//!
//! # Contents
//! - An isolated small cage exhausted by actual managed large cells.
//! - A group refill requiring an actual minor collection/promotion reserve.
//! - Unchanged live roots/headers and a subsequent legal canonical allocation.
//!
//! # Invariants
//! This integration binary owns its process-global cage. It uses only real
//! allocations and the collector's public engine plumbing, with no fake GC,
//! failure injection or replay hook. The group probe never creates a cell.
//!
//! # See also
//! - `heap::GcHeap::ensure_machine_allocation_with_roots` owns group admission.
//! - `scavenger` admits all promotion pages before its first forwarding write.

use otter_gc::page::{CELL_SIZE, LARGE_OBJECT_THRESHOLD, PAGE_PAYLOAD_SIZE};
use otter_gc::{GcHeap, HandleScope, OutOfMemory, RootScope, SafeTraceable, init_cage_with_size};

struct Cell {
    word: u64,
}
impl SafeTraceable for Cell {
    const TYPE_TAG: u8 = 0xec;
    fn trace_slots_safe(&mut self, _: &mut otter_gc::raw::SlotVisitor<'_>) {}
}

#[test]
fn failed_group_promotion_preserves_roots_and_a_smaller_canonical_source_prefix() {
    init_cage_with_size(16 * 1024 * 1024).expect("this isolated binary owns the small cage");
    let mut heap = GcHeap::new().expect("real semispaces");
    heap.set_gc_stress(0, false);
    let mut survivor = heap.alloc(Cell { word: 0x51a7 }).unwrap();
    let mut roots = RootScope::new(&mut heap);
    // SAFETY: this registered local stays stationary until roots drops.
    unsafe {
        roots.add_raw_slot((&mut survivor as *mut otter_gc::Gc<Cell>).cast());
    }
    heap.collect_minor_with_roots(&mut |_| {}).unwrap();
    let aged = survivor;
    // Each actual large allocation occupies a cage page. Retain the cells in
    // the nursery-independent large space; no collection occurs on cage OOM.
    // SAFETY: the heap-owned handle arena outlives every pressure handle.
    let pressure_scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let mut large = Vec::new();
    loop {
        match heap.alloc_trailing_with_roots(Cell { word: 0xdead }, 200 * 1024, &mut |_| {}) {
            Ok(value) => large.push(pressure_scope.local(value)),
            Err(OutOfMemory::CageExhausted) => break,
            Err(error) => panic!("unexpected real cage pressure: {error:?}"),
        }
    }
    assert!(!large.is_empty());
    let allocate = heap.always_allocate_scope();
    let cell_bytes = otter_gc::header::HEADER_SIZE + std::mem::size_of::<Cell>();
    // One half-page cell followed by a word leaves a short nonempty tail on
    // every existing from-space page. The next half-page cannot use that tail.
    for _ in 0..otter_gc::space::DEFAULT_NEW_SPACE_PAGES {
        heap.alloc_trailing_with_roots(
            Cell { word: 0x33 },
            LARGE_OBJECT_THRESHOLD - cell_bytes,
            &mut |_| {},
        )
        .unwrap();
        heap.alloc(Cell { word: 0x44 }).unwrap();
    }
    drop(allocate);
    heap.alloc(Cell { word: 0x55 })
        .expect("prime the real short LAB tail");
    let window = heap.machine_allocation_window();
    // SAFETY: one stable heap-owned initialized machine allocation window.
    let prior = unsafe { *window.lab };
    assert!(prior.remaining() >= cell_bytes);
    assert!(prior.remaining() < PAGE_PAYLOAD_SIZE);
    let tracked = heap.tracked_bytes();
    let before = heap.gc_stats().clone();
    assert!(
        !heap.oom_flag().load(std::sync::atomic::Ordering::Relaxed),
        "real cage pressure did not latch a heap-cap/source OOM before ensure"
    );
    heap.start_gc_pause_capture(4).unwrap();
    let error = heap
        .ensure_machine_allocation_with_roots(PAGE_PAYLOAD_SIZE, &mut |_| {})
        .unwrap_err();
    assert_eq!(error, OutOfMemory::CageExhausted);
    let capture = heap.take_gc_pause_capture().unwrap();
    assert_eq!(capture.records.len(), 1);
    assert_eq!(capture.records[0].kind, otter_gc::GcPauseKind::Minor);
    assert_eq!(
        capture.records[0].trigger,
        otter_gc::GcPauseTrigger::NurseryCapacity
    );
    assert_eq!(
        capture.records[0].outcome,
        otter_gc::GcPauseOutcome::CollectionFailed
    );
    assert_eq!(capture.dropped_records, 0);
    assert!(!capture.incomplete);
    assert_eq!(
        survivor, aged,
        "actual promotion refusal precedes root rewriting"
    );
    assert_eq!(heap.read_payload(survivor, |body| body.word), 0x51a7);
    // SAFETY: this root still names a live typed cell after preflight refusal.
    assert!(!unsafe { (*survivor.as_header_ptr()).is_forwarded() });
    assert_eq!(heap.tracked_bytes(), tracked);
    assert_eq!(heap.gc_stats().alloc_bytes_total, before.alloc_bytes_total);
    assert_eq!(heap.gc_stats().minor_gc_cycles, before.minor_gc_cycles);
    assert!(
        !heap.oom_flag().load(std::sync::atomic::Ordering::Relaxed),
        "group-sized failure cannot latch a source OOM"
    );
    let first = heap
        .alloc(Cell { word: 0xcafe })
        .expect("legal first source allocation in preserved tail");
    assert_eq!(heap.read_payload(first, |body| body.word), 0xcafe);
    assert_eq!(first.offset() as usize, prior.top as u32 as usize);
    assert_eq!(
        heap.gc_stats().alloc_bytes_total,
        before.alloc_bytes_total + cell_bytes as u64
    );
    assert_eq!(heap.gc_stats().minor_gc_cycles, before.minor_gc_cycles);
    assert_eq!(heap.read_payload(survivor, |body| body.word), 0x51a7);
    assert_eq!(heap.read_payload(large[0].get(), |body| body.word), 0xdead);
    assert!(cell_bytes.is_multiple_of(CELL_SIZE));
}
