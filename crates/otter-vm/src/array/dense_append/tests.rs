//! Plain dense append representation, reservation and barrier regressions.
//!
//! # Contents
//! - Fresh-copy and semantic-fallback premises over representation boundaries.
//! - COW isolation, same-array append and refusal without destination effects.
//! - Moving reservation, remembered reused slabs and immediate marking barriers.
//!
//! # Invariants
//! - Handle scopes retain every owner across allocation or collection.
//! - Source/destination caches are checked against their authoritative slabs.
//! - Marking assertions precede finish-mark root rescanning.
//!
//! # See also
//! - `array::append_plain_dense` owns the rooted operation.

use crate::{Value, array};
use otter_gc::raw::RawGc;
use otter_gc::{EmptyRoots, GcHeap, HandleScope, SafeTraceable, SitePlacement};

struct Child {
    marker: u64,
}

impl SafeTraceable for Child {
    const TYPE_TAG: u8 = 0xe7;
    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {}
}

fn old_array(heap: &mut GcHeap, mut values: Vec<Value>) -> array::JsArray {
    array::from_values_with_roots(heap, &mut values, SitePlacement::Old, &mut |_| {})
        .expect("old fixture shell and slab")
}

fn bits(heap: &GcHeap, array: array::JsArray) -> Vec<u64> {
    heap.read_payload(array, |body| {
        assert!(body.element_cache_is_current());
        assert_eq!(body.length(), body.dense_len());
        (0..body.dense_len())
            .map(|index| body.dense_value(index).unwrap().to_bits())
            .collect()
    })
}

fn append(heap: &mut GcHeap, dst: array::JsArray, src: array::JsArray) {
    // SAFETY: the heap outlives this nested scope. Its locals, unlike the
    // incoming Copy handles, are rewritten during the reserve collection.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let dst = scope.local(dst);
    let src = scope.local(src);
    assert!(array::append_plain_dense(dst.get(), src.get(), heap, &mut |_| {}).unwrap());
    heap.read_payload(dst.get(), |body| assert!(body.element_cache_is_current()));
    heap.read_payload(src.get(), |body| assert!(body.element_cache_is_current()));
}

fn marker(heap: &GcHeap, value: Value) -> u64 {
    let child = value.as_raw_gc().unwrap().checked_cast::<Child>().unwrap();
    heap.read_payload(child, |body| body.marker)
}

fn values(count: usize, kind: usize) -> Vec<Value> {
    (0..count)
        .map(|index| match kind {
            0 => Value::number_i32(index as i32 - 17),
            1 => match index % 6 {
                0 => Value::number_f64(-0.0),
                1 => Value::from_bits(crate::value::tag::box_double(0x7ff8_0000_0000_0317)),
                2 => Value::number_f64(f64::INFINITY),
                3 => Value::number_f64(f64::NEG_INFINITY),
                4 => Value::number_f64(f64::from_bits(1)),
                _ => Value::number_f64(index as f64 + 0.25),
            },
            _ => match index % 3 {
                0 => Value::boolean(true),
                1 => Value::undefined(),
                _ => Value::null(),
            },
        })
        .collect()
}

#[test]
fn dense_append_fresh_empty_preserves_numeric_and_tagged_values() {
    let mut heap = GcHeap::new().unwrap();
    // SAFETY: the heap outlives the handle scope and all of its locals.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    for count in [0, 1, 8, 9, 32, 33, 256] {
        for kind in 0..3 {
            let src = scope.local(old_array(&mut heap, values(count, kind)));
            let dst = scope.local(array::alloc_array_old_for_fixture(&mut heap).unwrap());
            let expected = bits(&heap, src.get());
            heap.read_payload(dst.get(), |body| {
                assert_eq!(body.dense_capacity(), 0);
                assert_eq!(body.length(), 0);
            });
            append(&mut heap, dst.get(), src.get());
            assert_eq!(
                bits(&heap, dst.get()),
                expected,
                "kind={kind} count={count}"
            );
            assert_eq!(bits(&heap, src.get()), expected);
            if count != 0 {
                let source_slab = heap.read_payload(src.get(), |body| body.slab);
                heap.read_payload(dst.get(), |body| {
                    assert_ne!(body.slab, source_slab);
                    assert_eq!(
                        body.dense_kind(),
                        if kind == 2 {
                            array::DenseElementKind::Tagged
                        } else {
                            array::DenseElementKind::PackedDouble
                        }
                    );
                });
            }
        }
    }
}

#[test]
fn dense_append_reused_and_mixed_destinations_keep_semantic_fallbacks() {
    let mut heap = GcHeap::new().unwrap();
    // SAFETY: the heap outlives every local in this scope.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    for source_kind in [0, 2] {
        for target_kind in [0, 2] {
            for capacity in [1, 16] {
                let src = scope.local(old_array(&mut heap, values(9, source_kind)));
                let dst = scope.local(old_array(&mut heap, values(capacity, target_kind)));
                array::set_length(dst.get(), &mut heap, 0).unwrap();
                let slab = heap.read_payload(dst.get(), |body| {
                    assert_eq!(body.dense_len(), 0);
                    assert_eq!(body.dense_capacity(), capacity);
                    body.slab
                });
                let expected = bits(&heap, src.get());
                append(&mut heap, dst.get(), src.get());
                assert_eq!(bits(&heap, dst.get()), expected);
                assert_eq!(bits(&heap, src.get()), expected);
                heap.read_payload(dst.get(), |body| {
                    assert_eq!(body.slab == slab, capacity >= 9);
                    assert_eq!(
                        body.dense_kind(),
                        if source_kind == 2 || target_kind == 2 {
                            array::DenseElementKind::Tagged
                        } else {
                            array::DenseElementKind::PackedDouble
                        }
                    );
                });
            }
        }
    }
    let src = scope.local(old_array(&mut heap, values(9, 0)));
    let dst = scope.local(old_array(&mut heap, vec![Value::boolean(false)]));
    let mut expected = bits(&heap, dst.get());
    expected.extend(bits(&heap, src.get()));
    append(&mut heap, dst.get(), src.get());
    assert_eq!(bits(&heap, dst.get()), expected);

    let gap = scope.local(array::alloc_array_old_for_fixture(&mut heap).unwrap());
    // Build the logical-only state directly: public small length growth
    // materializes holes, which would skip the logical-prefix fallback.
    heap.with_payload(gap.get(), |body| body.set_length(3));
    heap.read_payload(gap.get(), |body| {
        assert_eq!(body.length(), 3);
        assert_eq!(body.dense_len(), 0);
        assert_eq!(body.dense_capacity(), 0);
    });
    let mut expected = vec![Value::hole().to_bits(); 3];
    expected.extend(bits(&heap, src.get()));
    append(&mut heap, gap.get(), src.get());
    assert_eq!(bits(&heap, gap.get()), expected);
    assert_eq!(array::get(gap.get(), &heap, 0), Value::undefined());
    heap.read_payload(gap.get(), |body| {
        assert_eq!(body.dense_kind(), array::DenseElementKind::HoleyDouble);
    });

    let same = scope.local(old_array(&mut heap, values(33, 1)));
    let prefix = bits(&heap, same.get());
    let expected: Vec<_> = prefix.iter().chain(&prefix).copied().collect();
    append(&mut heap, same.get(), same.get());
    assert_eq!(bits(&heap, same.get()), expected);
}

#[test]
fn dense_append_keeps_shared_source_and_detaches_shared_destination() {
    let mut heap = GcHeap::new().unwrap();
    // SAFETY: the heap outlives all shared-slab and array roots.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let source_values = values(9, 0);
    let slab = scope.local(
        array::elements::alloc_shared_slab(
            &mut heap,
            &source_values,
            array::DenseElementKind::PackedDouble,
            &mut |_| {},
        )
        .unwrap(),
    );
    let src = scope.local(
        array::from_shared_slab(&mut heap, slab.get(), SitePlacement::Old, &mut |_| {}).unwrap(),
    );
    let sibling = scope.local(
        array::from_shared_slab(&mut heap, slab.get(), SitePlacement::Old, &mut |_| {}).unwrap(),
    );
    let dst = scope.local(array::alloc_array_old_for_fixture(&mut heap).unwrap());
    let prefix = bits(&heap, src.get());
    append(&mut heap, dst.get(), src.get());
    assert_eq!(bits(&heap, dst.get()), prefix);
    assert!(array::has_shared_elements(src.get(), &heap));
    assert!(array::has_shared_elements(sibling.get(), &heap));
    append(&mut heap, src.get(), sibling.get());
    assert!(!array::has_shared_elements(src.get(), &heap));
    assert!(array::has_shared_elements(sibling.get(), &heap));
    assert_eq!(bits(&heap, sibling.get()), prefix);
    assert_eq!(
        bits(&heap, src.get()),
        prefix.iter().chain(&prefix).copied().collect::<Vec<_>>()
    );
}

#[test]
fn dense_append_refuses_holes_logical_suffix_and_sidecar_before_effects() {
    let mut heap = GcHeap::new().unwrap();
    // SAFETY: the heap outlives the scope and every rooted array.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    for case in 0..4 {
        let src = scope.local(old_array(
            &mut heap,
            match case {
                0 => vec![Value::number_i32(1), Value::hole()],
                1 => vec![Value::boolean(true), Value::hole()],
                _ => values(9, 0),
            },
        ));
        if case == 2 {
            array::set_length(src.get(), &mut heap, 16).unwrap();
        }
        if case == 3 {
            array::set_prototype_override(src.get(), &mut heap, Some(Value::null())).unwrap();
        }
        let dst = scope.local(old_array(&mut heap, values(3, 2)));
        let before = bits(&heap, dst.get());
        let slab = heap.read_payload(dst.get(), |body| body.slab);
        let bytes = heap.gc_stats().alloc_bytes_total;
        assert!(!array::append_plain_dense(dst.get(), src.get(), &mut heap, &mut |_| {}).unwrap());
        assert_eq!(heap.gc_stats().alloc_bytes_total, bytes);
        assert_eq!(bits(&heap, dst.get()), before);
        heap.read_payload(dst.get(), |body| assert_eq!(body.slab, slab));
        heap.read_payload(src.get(), |body| assert!(body.element_cache_is_current()));
    }
}

#[test]
fn dense_append_reservation_relocates_source_and_pending_child() {
    let mut heap = GcHeap::new().unwrap();
    // SAFETY: all owners stay in this scope through the armed collection.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let dst = scope.local(array::alloc_array_old_for_fixture(&mut heap).unwrap());
    let src = scope.local(
        array::from_values_with_roots(
            &mut heap,
            &mut [Value::undefined(); 9],
            SitePlacement::Young(0),
            &mut |_| {},
        )
        .unwrap(),
    );
    let child = scope.local(heap.alloc(Child { marker: 731 }).unwrap());
    array::set(
        src.get(),
        &mut heap,
        8,
        Value::from_other_gc(child.get().raw()),
    )
    .unwrap();
    let before_source = src.get().offset();
    let before_child = child.get().offset();
    // SAFETY: live handles name the current headers.
    assert!(unsafe { (*src.get().as_header_ptr()).is_young() });
    assert!(unsafe { (*child.get().as_header_ptr()).is_young() });
    // Arm the next allocation specifically: append's sole reserve allocation.
    // This focused interval uses stride 1 even when the outer run uses 4/16.
    heap.set_gc_stress(1, false);
    let cycles = heap.gc_stats().minor_gc_cycles;
    append(&mut heap, dst.get(), src.get());
    assert!(heap.gc_stats().minor_gc_cycles > cycles);
    assert_ne!(src.get().offset(), before_source);
    assert_ne!(child.get().offset(), before_child);
    assert_eq!(marker(&heap, array::get(dst.get(), &heap, 8)), 731);
    assert_eq!(
        array::get(src.get(), &heap, 8),
        array::get(dst.get(), &heap, 8)
    );
    heap.collect_full(&mut |_| {}).unwrap();
    assert_eq!(marker(&heap, array::get(dst.get(), &heap, 8)), 731);
    heap.read_payload(src.get(), |body| assert!(body.element_cache_is_current()));
    heap.read_payload(dst.get(), |body| assert!(body.element_cache_is_current()));

    // The preceding interval isolates source/child movement with an old
    // destination. Now keep that source old and make the destination the last
    // fresh cell, so the same reserve owner must rewrite its own destination
    // handle as well as the helper's independently rooted copy.
    let moving_dst = scope.local(array::alloc_array_with_roots(&mut heap, &mut |_| {}).unwrap());
    let before_target = moving_dst.get().offset();
    // SAFETY: the just-created destination is rooted in this handle scope.
    assert!(unsafe { (*moving_dst.get().as_header_ptr()).is_young() });
    heap.set_gc_stress(1, false);
    let target_cycles = heap.gc_stats().minor_gc_cycles;
    let target_full = heap.gc_stats().gc_cycles;
    append(&mut heap, moving_dst.get(), src.get());
    assert_eq!(heap.gc_stats().minor_gc_cycles, target_cycles + 1);
    assert_eq!(heap.gc_stats().gc_cycles, target_full);
    assert_ne!(moving_dst.get().offset(), before_target);
    assert_eq!(bits(&heap, moving_dst.get()), bits(&heap, src.get()));
    assert_eq!(marker(&heap, array::get(moving_dst.get(), &heap, 8)), 731);
    heap.read_payload(moving_dst.get(), |body| {
        assert!(body.element_cache_is_current())
    });
}

#[test]
fn dense_append_reused_old_tagged_slab_remembers_only_owned_young_child() {
    let mut heap = GcHeap::new().unwrap();
    // SAFETY: the outer scope retains the destination after other roots drop.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let dst = scope.local(old_array(&mut heap, vec![Value::undefined(); 16]));
    array::set_length(dst.get(), &mut heap, 0).unwrap();
    let slab_before = heap.read_payload(dst.get(), |body| body.slab);
    // SAFETY: old_array explicitly places its slab in old space.
    assert!(unsafe { !(*slab_before.as_header_ptr()).is_young() });
    let child_before;
    {
        // SAFETY: the child scope drops before the outer scope and heap.
        let children = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let src = children.local(old_array(&mut heap, vec![Value::undefined(); 9]));
        let child = children.local(heap.alloc(Child { marker: 419 }).unwrap());
        child_before = child.get().offset();
        array::set(
            src.get(),
            &mut heap,
            8,
            Value::from_other_gc(child.get().raw()),
        )
        .unwrap();
        append(&mut heap, dst.get(), src.get());
        heap.read_payload(dst.get(), |body| assert_eq!(body.slab, slab_before));
        assert_eq!(child.get().offset(), child_before);
    }
    heap.collect_minor(EmptyRoots).unwrap();
    let first = array::get(dst.get(), &heap, 8);
    assert_ne!(first.as_raw_gc().unwrap().0, child_before);
    assert_eq!(marker(&heap, first), 419);
    heap.collect_minor(EmptyRoots).unwrap();
    assert_eq!(marker(&heap, array::get(dst.get(), &heap, 8)), 419);
    heap.collect_full(&mut |_| {}).unwrap();
    assert_eq!(marker(&heap, array::get(dst.get(), &heap, 8)), 419);
    heap.read_payload(dst.get(), |body| assert!(body.element_cache_is_current()));
}

#[test]
fn dense_append_fresh_tagged_copy_shades_white_old_children_before_finish_mark() {
    for tenured in [false, true] {
        let mut heap = GcHeap::new().unwrap();
        // SAFETY: the heap outlives the two source/destination root handles.
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let (source, child_offsets) = {
            // SAFETY: child handles protect construction and drop before marking.
            let children = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
            let child_handles: Vec<_> = (0..9)
                .map(|index| {
                    children.local(
                        heap.alloc_old(Child {
                            marker: index + 900,
                        })
                        .unwrap(),
                    )
                })
                .collect();
            let offsets: Vec<_> = child_handles
                .iter()
                .map(|child| child.get().offset())
                .collect();
            let values = child_handles
                .iter()
                .map(|child| Value::from_other_gc(child.get().raw()))
                .collect();
            (old_array(&mut heap, values), offsets)
        };
        // No allocation separates dropping child handles from rooting their owner.
        let src = scope.local(source);
        let dst = scope.local(array::alloc_array_old_for_fixture(&mut heap).unwrap());
        let source_slab = heap.read_payload(src.get(), |body| body.slab);
        // SAFETY: these rooted old cells remain live throughout the split cycle.
        assert!(unsafe { !(*src.get().as_header_ptr()).is_young() });
        assert!(unsafe { !(*source_slab.as_header_ptr()).is_young() });
        assert!(unsafe { !(*source_slab.as_header_ptr()).is_remembered() });
        heap.start_incremental_mark_phase(&mut |_| {}).unwrap();
        assert!(heap.marking().is_marking());
        assert!(!heap.marking().is_concurrent());
        assert!(!heap.is_marked(source_slab.raw()));
        for offset in &child_offsets {
            assert!(
                !heap.is_marked(RawGc(*offset)),
                "source-only child begins white"
            );
        }
        let full_cycles = heap.gc_stats().gc_cycles;
        // Exercise both existing placement paths. Tenuring selects old storage;
        // it does not change the configured stress flag, and this variant makes
        // no claim about executing the nursery stress-allocation path.
        heap.set_tenure_all(tenured);
        append(&mut heap, dst.get(), src.get());
        assert!(heap.marking().is_marking());
        assert!(!heap.marking().is_concurrent());
        assert_eq!(heap.gc_stats().gc_cycles, full_cycles);
        assert!(
            !heap.is_marked(source_slab.raw()),
            "the source graph was not drained during reservation"
        );
        let target_slab = heap.read_payload(dst.get(), |body| body.slab);
        // SAFETY: the destination roots its newly reserved slab.
        assert_eq!(
            unsafe { (*target_slab.as_header_ptr()).is_young() },
            !tenured
        );
        if tenured {
            // SAFETY: the destination owns this newly allocated old slab.
            assert_eq!(
                unsafe { (*target_slab.as_header_ptr()).mark_color() },
                otter_gc::MarkColor::Black
            );
        }
        // The explicit split cycle has no helper/drain. A possible stress minor
        // cannot trace the unremembered all-old source graph. The copy's insertion
        // barrier must shade every child now, before finish-mark rescans roots.
        for (index, offset) in child_offsets.iter().enumerate() {
            assert!(
                heap.is_marked(RawGc(*offset)),
                "copied child is shaded immediately"
            );
            assert_eq!(
                marker(&heap, array::get(dst.get(), &heap, index)),
                index as u64 + 900
            );
        }
        heap.finish_incremental_mark_phase(&mut |_| {}).unwrap();
        heap.sweep_phase();
        for index in 0..9 {
            assert_eq!(
                marker(&heap, array::get(dst.get(), &heap, index)),
                index as u64 + 900
            );
        }
    }
}
