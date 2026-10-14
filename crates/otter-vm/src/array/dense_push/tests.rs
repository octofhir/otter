//! Dense append representation changes, effect-free misses and remembered edges.
//!
//! # Contents
//! - Hole filling, numeric bit preservation and terminal tagged widening.
//! - Full-buffer, logical-gap and exotic-state refusal without writes.
//! - An old slab retaining an otherwise unrooted young child through collection.
//!
//! # Invariants
//! - Moving values are built and reread through production handle scopes.
//! - The child handle scope ends before collection; only its array edge remains.
//!
//! # See also
//! - [`super`] owns append publication and both generational barriers.

use super::*;
use crate::array::{self, DenseElementKind};
use crate::{Interpreter, NativeCallInfo, NativeCtx, NativeError};

fn check_cache(array: JsArray, heap: &GcHeap, expected: DenseElementKind) {
    heap.read_payload(array, |body| {
        assert!(body.element_cache_is_current());
        assert_eq!(body.dense_kind(), expected);
    });
}

#[test]
fn append_preserves_holes_numeric_bits_and_tagged_widening() {
    let mut heap = GcHeap::new().unwrap();
    let array = array::from_elements_old_for_fixture(
        &mut heap,
        [Value::number_f64(-0.0), Value::number_f64(f64::NAN)],
    )
    .unwrap();
    let _ = array::pop(array, &mut heap);
    assert_eq!(try_push_spare(array, &mut heap, Value::hole()), Some(2));
    check_cache(array, &heap, DenseElementKind::HoleyDouble);
    let _ = array::pop(array, &mut heap);
    assert_eq!(
        try_push_spare(array, &mut heap, Value::number_f64(f64::NAN)),
        Some(2)
    );
    check_cache(array, &heap, DenseElementKind::PackedDouble);
    assert_eq!(
        array::get(array, &heap, 0).as_f64().unwrap().to_bits(),
        (-0.0f64).to_bits()
    );
    assert!(array::get(array, &heap, 1).as_f64().unwrap().is_nan());
    // Growth uses the same append owner, including its numeric-to-tagged change.
    array::push(array, &mut heap, Value::boolean(true)).unwrap();
    check_cache(array, &heap, DenseElementKind::Tagged);
    assert_eq!(array::get(array, &heap, 2), Value::boolean(true));
    assert_eq!(
        array::get(array, &heap, 0).as_f64().unwrap().to_bits(),
        (-0.0f64).to_bits()
    );
    assert!(array::get(array, &heap, 1).as_f64().unwrap().is_nan());
}

#[test]
fn misses_preserve_the_full_buffer_gap_and_readonly_receiver() {
    let mut heap = GcHeap::new().unwrap();
    let array = array::from_elements_old_for_fixture(&mut heap, [Value::number_i32(3)]).unwrap();
    while array::len(array, &heap) < heap.read_payload(array, |body| body.dense_capacity()) {
        array::push(array, &mut heap, Value::number_i32(4)).unwrap();
    }
    let before = heap.read_payload(array, |body| body.dense_values());
    assert_eq!(try_push_spare(array, &mut heap, Value::boolean(true)), None);
    assert_eq!(heap.read_payload(array, |body| body.dense_values()), before);
    let _ = array::pop(array, &mut heap);
    array::set_length(array, &mut heap, 1 << 21).unwrap();
    let dense = heap.read_payload(array, |body| body.dense_values());
    assert_eq!(try_push_spare(array, &mut heap, Value::boolean(true)), None);
    assert_eq!(array::len(array, &heap), 1 << 21);
    assert_eq!(heap.read_payload(array, |body| body.dense_values()), dense);
    array::set_length(array, &mut heap, 0).unwrap();
    array::set_length_writable(array, &mut heap, false).unwrap();
    assert_eq!(try_push_spare(array, &mut heap, Value::boolean(true)), None);
    assert_eq!(array::len(array, &heap), 0);
    assert!(!array::length_writable(array, &heap));
    check_cache(array, &heap, DenseElementKind::PackedDouble);
}

#[test]
fn old_slab_append_remembers_a_child_after_its_handle_scope_ends() {
    let mut interp = Interpreter::new().unwrap();
    interp.gc_heap_mut().set_gc_stress(0, true);
    NativeCtx::with_host_context(&mut interp, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let array = scope.array(0)?;
            let initial = scope.number(1.0);
            scope.set_index(array, 0, initial)?;
            scope.with_turn_parts(|interp, _| interp.force_gc().unwrap());
            let old_child = scope.scope(|mut child_scope| {
                let child = child_scope.object()?;
                let marker = child_scope.number(731.0);
                child_scope.set(child, "marker", marker)?;
                let offset = child_scope.with_turn_parts(|interp, _| {
                    let array = interp.escape_scoped(array).as_array().unwrap();
                    let child = interp.escape_scoped(child);
                    assert_eq!(try_push_spare(array, interp.gc_heap_mut(), child), Some(2));
                    child.as_object().unwrap().offset()
                });
                Ok::<_, NativeError>(offset)
            })?;
            let before = scope.with_turn_parts(|interp, _| {
                interp.gc_heap_mut().set_gc_stress(1, true);
                interp.gc_heap_mut().gc_stats().clone()
            });
            for _ in 0..8 {
                scope.scope(|mut temporary| temporary.object().map(|_| ()))?;
            }
            scope.with_turn_parts(|interp, _| {
                let array = interp.escape_scoped(array).as_array().unwrap();
                let after = interp.gc_heap_mut().gc_stats().clone();
                let heap = interp.gc_heap();
                let child = array::get(array, heap, 1).as_object().unwrap();
                assert_ne!(child.offset(), old_child, "young child actually moved");
                assert_eq!(
                    crate::object::get(child, heap, "marker"),
                    Some(Value::number_i32(731))
                );
                assert!(after.minor_gc_cycles > before.minor_gc_cycles);
                assert!(after.minor_slot_updates > before.minor_slot_updates);
                check_cache(array, heap, DenseElementKind::Tagged);
                interp.gc_heap_mut().set_gc_stress(0, true);
            });
            Ok::<_, NativeError>(())
        })
    })
    .unwrap();
}
