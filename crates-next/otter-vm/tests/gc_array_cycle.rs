//! Array GC regressions for task 78.
//!
//! Arrays are GC-managed `Gc<ArrayBody>` handles. Dense elements can
//! point back to the array itself, so mark-sweep must reclaim the
//! cycle when no runtime root retains the array.

use otter_vm::Interpreter;
use otter_vm::Value;
use otter_vm::array::ARRAY_BODY_TYPE_TAG;

#[test]
fn array_self_reference_reaped() {
    let mut interp = Interpreter::new();

    interp.force_gc();
    let baseline = interp.gc_heap_mut().gc_stats().by_type[ARRAY_BODY_TYPE_TAG as usize].live_bytes;

    let arr = otter_vm::array::alloc_array(interp.gc_heap_mut()).expect("alloc array");
    otter_vm::array::push(arr, interp.gc_heap_mut(), Value::Array(arr)).expect("push self");

    let with_array =
        interp.gc_heap_mut().gc_stats().by_type[ARRAY_BODY_TYPE_TAG as usize].live_bytes;
    assert!(
        with_array > baseline,
        "array allocation must bump live_bytes (baseline={baseline}, with_array={with_array})"
    );

    let _ = arr;
    interp.force_gc();
    let after = interp.gc_heap_mut().gc_stats().by_type[ARRAY_BODY_TYPE_TAG as usize].live_bytes;
    assert_eq!(
        after, baseline,
        "self-referential array must be reaped by force_gc (baseline={baseline}, after={after})"
    );
}

#[test]
fn array_dense_storage_cap_kicks_in() {
    let mut heap = otter_gc::GcHeap::with_max_heap_bytes(4 * 1024 * 1024).expect("gc heap");
    let values = std::iter::repeat_n(Value::Undefined, 1 << 20);
    let err = otter_vm::array::from_elements(&mut heap, values).expect_err("array must exceed cap");
    assert!(
        matches!(err, otter_gc::OutOfMemory::HeapCapExceeded { .. }),
        "expected HeapCapExceeded, got {err:?}"
    );
}
