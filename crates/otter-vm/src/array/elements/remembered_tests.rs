//! Remembered dense-array tracing regressions.
//!
//! # Contents
//! - Linear append scanning with geometric slab growth.
//! - Roots that evacuate a child before its remembered parent is traced.
//! - Arbitrary stores, bulk rewrites, truncation, and full marking.
//!
//! # Invariants
//! - Every test keeps the old array in a handle scope across collections.
//! - Assertions inspect payloads after actual relocation, not only counters.
//!
//! # See also
//! - [`super::ElementSlabBody`]

use crate::{Value, array};
use otter_gc::{EmptyRoots, GcHeap, HandleScope, SafeTraceable};

struct Child {
    marker: u64,
}

impl SafeTraceable for Child {
    const TYPE_TAG: u8 = 0xe7;

    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {}
}

fn child_value(heap: &mut GcHeap, marker: u64) -> Value {
    Value::from_other_gc(heap.alloc(Child { marker }).expect("child").raw())
}

fn assert_marker(heap: &GcHeap, value: Value, marker: u64) {
    let child = value
        .as_raw_gc()
        .and_then(|raw| raw.checked_cast::<Child>())
        .expect("live child");
    assert_eq!(heap.read_payload(child, |body| body.marker), marker);
}

#[test]
fn remembered_appends_scan_linear_slots_through_growth() {
    const COUNT: usize = 1024;
    let mut heap = GcHeap::new().expect("heap");
    // SAFETY: heap outlives the scope and its handles.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let array = scope.local(array::alloc_array_old_for_fixture(&mut heap).expect("array"));
    let before = heap.gc_stats().minor_slots_scanned;

    for index in 0..COUNT {
        let value = child_value(&mut heap, index as u64);
        array::push(array.get(), &mut heap, value).expect("append");
        heap.collect_minor(EmptyRoots).expect("moving collection");
    }

    let scanned = heap.gc_stats().minor_slots_scanned - before;
    assert!(
        scanned <= (COUNT * 8) as u64,
        "appending {COUNT} children must not re-trace each old prefix: {scanned} slots"
    );
    assert!(heap.gc_stats().minor_gc_cycles >= COUNT as u64);
    // Full marking must continue to walk clean old slots after dirty ranges
    // have been consumed by the preceding minor collections.
    heap.collect_full(&mut |_| {}).expect("full collection");
    for index in 0..COUNT {
        assert_marker(&heap, array::get(array.get(), &heap, index), index as u64);
    }
}

#[test]
fn remembered_slot_keeps_child_already_evacuated_by_root() {
    let mut heap = GcHeap::new().expect("heap");
    // SAFETY: heap outlives both nested scopes and their handles.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let array = scope.local(array::alloc_array_old_for_fixture(&mut heap).expect("array"));
    array::push(array.get(), &mut heap, Value::undefined()).expect("reserve slot");
    let copied_offset;
    {
        // SAFETY: the nested scope ends before the heap and outer scope.
        let child_scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let child = child_scope.local(heap.alloc(Child { marker: 79 }).expect("child"));
        let original_offset = child.get().offset();
        array::set(
            array.get(),
            &mut heap,
            0,
            Value::from_other_gc(child.get().raw()),
        )
        .expect("store child");
        heap.collect_minor(EmptyRoots).expect("first relocation");
        copied_offset = child.get().offset();
        assert_ne!(copied_offset, original_offset);
        // SAFETY: the live handle points at the relocated child header.
        assert!(unsafe { (*child.get().as_header_ptr()).is_young() });
        assert_marker(&heap, array::get(array.get(), &heap, 0), 79);
    }

    // Only the array owns the child now; retaining the still-young dirty slot
    // from the first remembered trace is necessary to survive this move.
    heap.collect_minor(EmptyRoots).expect("second relocation");
    let value = array::get(array.get(), &heap, 0);
    assert_ne!(value.as_raw_gc().expect("child").0, copied_offset);
    assert_marker(&heap, value, 79);
}

#[test]
fn remembered_ranges_follow_overwrites_bulk_rewrites_and_truncation() {
    let mut heap = GcHeap::new().expect("heap");
    // SAFETY: heap outlives the scope and its handles.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let array = scope.local(array::alloc_array_old_for_fixture(&mut heap).expect("array"));
    for index in 0..16 {
        array::push(array.get(), &mut heap, Value::number_i32(index)).expect("numeric prefix");
    }
    let first = child_value(&mut heap, 31);
    array::set(array.get(), &mut heap, 3, first).expect("widen middle slot");
    let removed = child_value(&mut heap, 99);
    array::set(array.get(), &mut heap, 14, removed).expect("distant dirty slot");
    array::with_elements_rewrite(array.get(), &mut heap, |values| values.rotate_left(1));
    array::set_length(array.get(), &mut heap, 8).expect("truncate dirty suffix");
    array::set(array.get(), &mut heap, 31, Value::undefined()).expect("grow copied slab");
    let last = child_value(&mut heap, 61);
    array::set(array.get(), &mut heap, 29, last).expect("new slab dirty slot");

    heap.collect_minor(EmptyRoots).expect("moving collection");
    heap.collect_full(&mut |_| {}).expect("full collection");
    assert_marker(&heap, array::get(array.get(), &heap, 2), 31);
    assert_marker(&heap, array::get(array.get(), &heap, 29), 61);
    assert!(array::get(array.get(), &heap, 13).is_undefined());
    assert_eq!(array::len(array.get(), &heap), 32);
}
