//! Array cache rebasing during read-only and moving root visits.
//!
//! # Contents
//! - A garbage old array with an intentionally unbarriered stale slab.
//! - Real slab evacuation through an independent handle-scope root.
//!
//! # Invariants
//! - The old array has no incoming heap edge and remains unremembered.
//! - The tracer visits its slot without reading retired child metadata.
//! - Fault injection writes retired storage; the fixture never reads it.
//!
//! # See also
//! - `otter_gc::heap::verification` filters incoming owners before child reads.
//! - `dense_guard::tests` covers moving caches and indexed snapshot restore.

use super::{ArrayBody, elements};
use otter_gc::{GcHeap, HandleScope, SafeTraceable, SitePlacement};

#[test]
fn unreferenced_old_array_trace_does_not_read_its_stale_slab() {
    let mut heap = GcHeap::new().expect("fixture heap");
    // SAFETY: the heap and its handle arena outlive this scope.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    // Protect preparation under an inherited full-stress policy as well.
    // Only the slab remains rooted during the measured minor below.
    let (garbage, slab) = {
        // SAFETY: this nested scope closes before the outer slab root is made.
        let preparation = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let garbage = preparation.local(heap.alloc_old(ArrayBody::default()).expect("old array"));
        let slab = elements::alloc_element_slab(
            &mut heap,
            1,
            elements::DenseElementKind::Tagged,
            SitePlacement::Young(0),
            &mut |_| {},
        )
        .expect("independently rooted young slab");
        (garbage.get(), slab)
    };
    let slab = scope.local(slab);
    let before = slab.get().offset();
    let base_before = elements::data_base(slab.get());
    // SAFETY: test-owned old body, no intervening collection. Deliberately
    // omit the barrier: the unreferenced garbage holder must be filtered by
    // verification, while the independent handle keeps the slab alive.
    unsafe {
        let body = garbage
            .as_header_ptr()
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<ArrayBody>();
        (*body).adopt_slab(slab.get(), 0);
        assert!(!(*garbage.as_header_ptr()).is_remembered());
        assert!((*slab.get().as_header_ptr()).is_young());
    }
    let cycles = heap.gc_cycle_counts();
    heap.collect_minor_with_roots(&mut |_| {})
        .expect("actual slab evacuation");
    assert_ne!(slab.get().offset(), before);
    heap.read_payload(slab.get(), |body| assert_eq!(body.capacity(), 1));
    // Reuse one word in retired from-space. The live slab still has capacity
    // one; a full semantic-cache refresh would incorrectly publish nine from
    // the garbage body's stale child. No retired payload is read here.
    // SAFETY: the completed minor retains this inactive nursery storage; no
    // allocation, collector or helper can use it during this test-owned write.
    unsafe {
        base_before
            .sub(std::mem::size_of::<elements::ElementSlabBody>())
            .add(elements::SLAB_CAPACITY_BYTE)
            .cast::<u32>()
            .write(9);
    }
    // SAFETY: no full collection reclaimed the old body. No allocation or
    // collection occurs while it is borrowed. Its stale child is visited as
    // a slot only; a read-only visitor must not provoke a payload/header read.
    let body = unsafe {
        &mut *garbage
            .as_header_ptr()
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<ArrayBody>()
    };
    let mut visits = 0;
    body.trace_slots_safe(&mut |slot| {
        visits += 1;
        // SAFETY: the tracer supplied an initialized slot of the old body.
        assert_eq!(unsafe { (*slot).0 }, before);
    });
    assert_eq!(visits, 1);
    assert_eq!(body.elements_ptr.get(), base_before);
    assert_eq!(
        body.dense_capacity(),
        1,
        "trace must not read retired metadata"
    );
    assert_eq!(body.dense_kind(), elements::DenseElementKind::Tagged);
    assert_eq!(heap.gc_cycle_counts(), (cycles.0 + 1, cycles.1));
}
