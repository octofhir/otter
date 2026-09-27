//! Moving-child coverage for inline closure captures.
//!
//! # Contents
//! - A young binding reachable only through the closure's trailing array.
//!
//! # Invariants
//! - Allocation publication records tail edges before a minor collection.
//! - The native capture base addresses the collector-rewritten slots.

use super::*;

#[test]
fn inline_capture_is_rewritten_through_two_minor_collections() {
    let mut heap = GcHeap::new().expect("heap");
    // SAFETY: the scope ends before its heap and owns the only closure root.
    let scope = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let cell = heap
        .alloc(crate::UpvalueCellBody {
            value: Value::number_i32(73),
        })
        .expect("young binding");
    let initial_offset = cell.offset();
    let closure =
        alloc_closure(&mut heap, 7, &mut [cell], None, None, None, None).expect("inline closure");
    let rooted = scope.local(closure.handle());
    let base = closure.call_header(&heap).upvalue_base;
    let copied_offset;
    {
        // SAFETY: the child scope ends before the heap and outer closure root.
        let child_scope = unsafe { otter_gc::HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let child = child_scope.local(closure.upvalues_snapshot(&heap)[0]);
        heap.collect_minor(otter_gc::EmptyRoots)
            .expect("move child");
        copied_offset = child.get().offset();
        assert_ne!(copied_offset, initial_offset);
        // A root evacuates the child before the old closure's remembered scan.
        // SAFETY: the rooted handle names the live, relocated cell header.
        assert!(unsafe { (*child.get().as_header_ptr()).is_young() });
        let current = JsClosure::from_parts(rooted.get(), 7);
        let cell = current.call_state(&heap).upvalues.read(0).expect("capture");
        assert_eq!(cell, child.get());
        assert_eq!(crate::read_upvalue(&heap, cell), Value::number_i32(73));
        assert_eq!(current.call_header(&heap).upvalue_base, base);
        assert_eq!(current.upvalues_snapshot(&heap), vec![cell]);
    }
    // Only the closure owns the still-young binding now. Its remembered tail
    // must retain the edge after the first collection and rewrite it again.
    heap.collect_minor(otter_gc::EmptyRoots)
        .expect("move child again");
    let current = JsClosure::from_parts(rooted.get(), 7);
    let cell = current.call_state(&heap).upvalues.read(0).expect("capture");
    assert_ne!(cell.offset(), copied_offset);
    assert_eq!(crate::read_upvalue(&heap, cell), Value::number_i32(73));
    assert_eq!(current.call_header(&heap).upvalue_base, base);
    heap.collect_full(&mut |_| {}).expect("full collection");
    let current = JsClosure::from_parts(rooted.get(), 7);
    assert_eq!(
        crate::read_upvalue(&heap, current.upvalues_snapshot(&heap)[0]),
        Value::number_i32(73)
    );
}
