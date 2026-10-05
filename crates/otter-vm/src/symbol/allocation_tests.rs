//! Symbol allocation must publish the collector-updated description.
//!
//! # Contents
//! - All four body-allocating symbol constructors under actual heap-cap GC.
//! - Exact old-body, wrapper-cache and independently rooted description aliases.
//! - A later moving minor collection with the old symbol as the only owner.
//!
//! # Invariants
//! A genuine dead young cell funds successful collection inside the tested old
//! allocation. Setup neither collects nor clears an OOM flag. Handles live in
//! the heap's standard arena; the second collection has no description root.
//! Physical footprints come from the sole body types and GC alignment rules.
//!
//! # See also
//! - `super::alloc_symbol` and `alloc_private_name_symbol` own body publication.
//! - `JsSymbol::from_handle` owns wrapper-cache initialization.

use super::*;
use otter_gc::{GcHeap, GcPauseKind, GcPauseOutcome, GcPauseTrigger, HandleScope, SafeTraceable};

struct FundingCell {
    word: u64,
}
impl SafeTraceable for FundingCell {
    const TYPE_TAG: u8 = 0xea;
    fn trace_slots_safe(&mut self, _: &mut otter_gc::raw::SlotVisitor<'_>) {}
}
#[derive(Clone, Copy, Debug)]
enum Constructor {
    Ordinary,
    Private,
    WellKnown,
    Registered,
}
impl Constructor {
    fn allocate(self, heap: &mut GcHeap, description: JsString) -> JsSymbol {
        match self {
            Self::Ordinary => JsSymbol::new(heap, Some(description)),
            Self::Private => JsSymbol::new_private(heap, Some(description)),
            Self::WellKnown => JsSymbol::well_known(heap, WellKnown::Iterator, description),
            Self::Registered => JsSymbol::registered(heap, description),
        }
        .expect("the actual collection reclaims enough funding for this symbol")
    }
    fn check(self, symbol: JsSymbol) {
        assert_eq!(symbol.is_private_name(), matches!(self, Self::Private));
        assert_eq!(symbol.is_registered(), matches!(self, Self::Registered));
        assert_eq!(
            symbol.well_known_tag(),
            if matches!(self, Self::WellKnown) {
                Some(WellKnown::Iterator)
            } else {
                None
            }
        );
    }
}
fn footprint<T>() -> u64 {
    otter_gc::page::align_up(
        otter_gc::header::HEADER_SIZE + std::mem::size_of::<T>(),
        otter_gc::page::CELL_SIZE,
    ) as u64
}
fn run(constructor: Constructor) {
    let cap = 64 * 1024u64;
    let mut heap = GcHeap::with_max_heap_bytes(cap).expect("physical heap");
    heap.set_gc_stress(0, true);
    // SAFETY: the heap's persistent handle stack outlives both scopes below.
    let outer = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let (symbol, reserved, moved) = {
        // SAFETY: nested description handles cannot outlive this scope.
        let descriptions = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let description = JsString::from_str("moving description", &mut heap).unwrap();
        let live = descriptions.local(description.handle());
        let alias = descriptions.local(description.handle());
        let original = live.get().offset();
        let setup_cycles = heap.gc_cycle_counts();
        let funding = heap.alloc(FundingCell { word: 0xf00d }).unwrap();
        heap.read_payload(funding, |cell| assert_eq!(cell.word, 0xf00d));
        let allocated = heap.stats().allocated_bytes as u64;
        let symbol_bytes = footprint::<SymbolBody>();
        let funding_bytes = footprint::<FundingCell>();
        assert!(symbol_bytes > funding_bytes && funding_bytes > 1);
        // One-byte over-admission forces the old SymbolBody allocator through
        // real full collection. Reclaiming the dead funding cell admits it.
        let reserved = cap - allocated - symbol_bytes + 1;
        heap.reserve_bytes_no_collect(reserved)
            .expect("setup reservation fits actual current cells");
        assert_eq!(
            heap.gc_cycle_counts(),
            setup_cycles,
            "all descriptions remain freshly young"
        );
        assert_eq!(live.get().offset(), original);
        assert_eq!(live.get(), alias.get());
        assert_eq!(
            heap.tracked_bytes() + symbol_bytes,
            cap + 1,
            "actual tested allocation must collect"
        );
        assert!(!heap.oom_flag().load(std::sync::atomic::Ordering::Relaxed));
        heap.start_gc_pause_capture(4).unwrap();
        let current = JsString::from_handle(live.get(), &heap);
        let symbol = constructor.allocate(&mut heap, current);
        let capture = heap.take_gc_pause_capture().unwrap();
        assert!(!capture.incomplete);
        assert_eq!(capture.dropped_records, 0);
        assert_eq!(capture.records.len(), 1);
        assert_eq!(capture.records[0].kind, GcPauseKind::Full);
        assert_eq!(capture.records[0].trigger, GcPauseTrigger::HeapCap);
        assert_eq!(capture.records[0].outcome, GcPauseOutcome::Completed);
        assert!(
            heap.gc_cycle_counts().1 > setup_cycles.1,
            "collection occurred inside the tested symbol allocation"
        );
        assert_ne!(
            live.get().offset(),
            original,
            "young description actually moved in that allocation"
        );
        assert_eq!(live.get(), alias.get());
        let actual = heap.read_payload(symbol.handle(), |body| body.description.unwrap().handle());
        assert_eq!(
            actual,
            live.get(),
            "completed pending body keeps the collector-updated description"
        );
        assert_eq!(
            symbol.description().unwrap().handle(),
            actual,
            "returned wrapper cannot retain the pre-GC copy"
        );
        assert_eq!(
            symbol.description().unwrap().to_lossy_string(&heap),
            "moving description"
        );
        constructor.check(symbol);
        assert_eq!(heap.stats().reserved_bytes, reserved);
        assert_eq!(
            heap.tracked_bytes(),
            cap + 1 - funding_bytes,
            "successful source allocation charges its exact footprint once"
        );
        assert!(!heap.oom_flag().load(std::sync::atomic::Ordering::Relaxed));
        // SAFETY: the old-space symbol is live; allocation's publication is
        // complete and no intervening collection occurred.
        assert!(unsafe { (*symbol.handle().as_header_ptr()).is_old() });
        (symbol, reserved, actual.offset())
    };
    // The description scope is gone. Only this old symbol body now owns its
    // moving young description, so the next minor requires its remembered edge.
    let owner = outer.local(symbol.handle());
    assert_eq!(heap.handle_stack().len(), 1);
    heap.release_bytes(reserved);
    let before = heap.gc_cycle_counts();
    heap.collect_minor_with_roots(&mut |_| {}).unwrap();
    assert!(heap.gc_cycle_counts().0 > before.0);
    let rebuilt = JsSymbol::from_handle(&heap, owner.get());
    constructor.check(rebuilt);
    assert_ne!(
        rebuilt.description().unwrap().handle().offset(),
        moved,
        "remembered old body edge rewrites the second moving description"
    );
    assert_eq!(
        rebuilt.description().unwrap().to_lossy_string(&heap),
        "moving description"
    );
    assert_eq!(
        heap.read_payload(owner.get(), |body| body.description.unwrap().handle()),
        rebuilt.description().unwrap().handle()
    );
}

#[test]
fn symbol_allocation_reloads_description_after_actual_cap_collection_and_keeps_its_old_edge() {
    for constructor in [
        Constructor::Ordinary,
        Constructor::Private,
        Constructor::WellKnown,
        Constructor::Registered,
    ] {
        run(constructor);
    }
}
