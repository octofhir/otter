//! Pending Array.fromAsync targets and abrupt-close ownership.
//!
//! # Contents
//! - Both ordered publication paths retain unpublished arrays through real cap GC.
//! - Length reservation returns the scoped array's current relocated value.
//! - Catchable close throws preserve the original rejection; actual OOM propagates.
//!
//! # Invariants
//! - Every claimed collection is performed by the observed production operation.
//! - No external handle roots the unpublished target in the publication proof.
//! - Roots and callbacks use the existing interpreter/NativeCtx handle owners.
//!
//! # See also
//! - `super::Interpreter::collect_publish_array_state` owns pending publication.
//! - `crate::handles::Interpreter::scoped_array` owns array length preparation.

use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicI32, AtomicUsize, Ordering},
};

struct Funding([u64; 16384]);
impl otter_gc::SafeTraceable for Funding {
    const TYPE_TAG: u8 = 0xe4;
    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        let _ = self.0[0];
    }
}

fn put(vm: &mut Interpreter, target: crate::Local<'_>, key: &str, value: Value) {
    let mut object = vm.escape_scoped(target).as_object().expect("state object");
    vm.create_data_property(&mut object, key, value)
        .expect("fixture field");
}

#[test]
fn unpublished_arrays_move_through_both_ordered_state_publications_repeatedly() {
    for iterable in [true, false] {
        let cap = 4 * 1024 * 1024;
        let mut vm = Interpreter::with_string_heap_cap(cap).expect("fromAsync bootstrap");
        vm.gc_heap.set_gc_stress(0, false);
        let context = vm
            .link_module(
                crate::test_support::minimal_bytecode_module("fromAsync-pending-array"),
                crate::source_registry::SourceRegistry::default(),
            )
            .expect("actual function owner");
        for round in 0..3 {
            vm.with_handle_scope(|vm, scope| {
                let state = vm.scoped_object_bare(scope).expect("state");
                // A distinct prepared prefix makes the next production field
                // transition cold on every round; no cached shape skips GC.
                put(
                    vm,
                    state,
                    &format!("pending-{iterable}-{round}"),
                    Value::number_i32(round),
                );
                let source = vm
                    .scoped_object_bare(scope)
                    .expect("iterator or array-like");
                put(vm, source, "marker", Value::number_i32(719 + round));
                let source_value = vm.escape_scoped(source);
                let alias = vm.scoped_value(scope, source_value);
                let setup_cycles = vm.gc_heap.gc_cycle_counts();
                let st = vm.push_iteration_anchor(vm.escape_scoped(state)) - 1;
                vm.gc_heap
                    .alloc_old(Funding([0; 16384]))
                    .expect("dead cap funding");
                // The returned target has no caller handle or iteration anchor.
                // Only the production publication scope may keep it alive.
                let array = vm
                    .collect_new_array(if iterable { 0 } else { 7 })
                    .expect("unpublished target");
                let before_array = array.as_array().unwrap().offset();
                assert_eq!(
                    vm.gc_heap.gc_cycle_counts(),
                    setup_cycles,
                    "fresh young inputs before publication"
                );
                let before_source = vm.escape_scoped(source).as_object().unwrap().offset();
                let before = vm.gc_heap.gc_cycle_counts();
                let reserve = cap - vm.gc_heap.tracked_bytes();
                vm.gc_heap
                    .reserve_bytes_no_collect(reserve)
                    .expect("full cap without collection");
                assert_eq!(vm.gc_heap.gc_cycle_counts(), before);
                let source_value = vm.escape_scoped(source);
                let fields = if iterable {
                    [
                        (slot::ITERATOR, source_value),
                        (slot::NEXT, Value::function(context.function_base())),
                    ]
                } else {
                    [
                        (slot::ARRAY_LIKE, source_value),
                        (slot::LENGTH, Value::number_i32(7)),
                    ]
                };
                let result = vm.collect_publish_array_state(st, array, &fields);
                vm.gc_heap.release_bytes(reserve);
                result.expect("collecting state publication");
                assert!(
                    vm.gc_heap.gc_cycle_counts().1 > before.1,
                    "actual publication full GC"
                );
                let state_value = vm.escape_scoped(state);
                let stored = vm
                    .state_get(state_value, slot::ARRAY)
                    .as_array()
                    .expect("published target");
                assert_ne!(
                    stored.offset(),
                    before_array,
                    "actual unpublished array relocation"
                );
                assert_eq!(
                    crate::array::len(stored, vm.gc_heap()),
                    if iterable { 0 } else { 7 }
                );
                assert_ne!(
                    vm.escape_scoped(source).as_object().unwrap().offset(),
                    before_source,
                    "actual source relocation through pending fields"
                );
                assert_eq!(vm.escape_scoped(source), vm.escape_scoped(alias));
                assert_eq!(
                    vm.state_get(state_value, fields[0].0),
                    vm.escape_scoped(source)
                );
                assert_eq!(vm.state_get(state_value, fields[1].0), fields[1].1);
                assert_eq!(
                    crate::object::get_own(
                        vm.escape_scoped(source).as_object().unwrap(),
                        vm.gc_heap(),
                        "marker"
                    ),
                    Some(Value::number_i32(719 + round))
                );
                vm.force_gc()
                    .expect("second real collection after publication");
                let state_value = vm.escape_scoped(state);
                let stored = vm
                    .state_get(state_value, slot::ARRAY)
                    .as_array()
                    .expect("retained target");
                assert_eq!(
                    crate::array::len(stored, vm.gc_heap()),
                    if iterable { 0 } else { 7 }
                );
                assert_eq!(
                    vm.state_get(state_value, fields[0].0),
                    vm.escape_scoped(source)
                );
                assert_eq!(vm.escape_scoped(source), vm.escape_scoped(alias));
                vm.pop_iteration_anchors_to(st);
                assert_eq!(vm.gc_heap.stats().reserved_bytes, 0);
            });
        }
    }
}

#[test]
fn array_length_reservation_returns_the_current_scoped_array_after_cap_collection() {
    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("length bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    for _ in 0..3 {
        vm.with_handle_scope(|vm, scope| {
            vm.gc_heap
                .alloc_old(Funding([0; 16384]))
                .expect("dead cap funding");
            let reserve = cap - vm.gc_heap.tracked_bytes();
            vm.gc_heap
                .reserve_bytes_no_collect(reserve)
                .expect("length cap pressure");
            let before = vm.gc_heap.gc_cycle_counts();
            let result = vm.collect_new_array(512);
            vm.gc_heap.release_bytes(reserve);
            let value = result.expect("collecting length growth");
            let result = vm.scoped_value(scope, value);
            assert!(
                vm.gc_heap.gc_cycle_counts().1 > before.1,
                "actual length preparation full GC"
            );
            let array = vm
                .escape_scoped(result)
                .as_array()
                .expect("current array body");
            assert_eq!(crate::array::len(array, vm.gc_heap()), 512);
            assert!(crate::array::get(array, vm.gc_heap(), 511).is_undefined());
            assert!(!crate::array::has_own_element(array, vm.gc_heap(), 511));
            vm.force_gc().expect("retained grown array");
            let array = vm
                .escape_scoped(result)
                .as_array()
                .expect("array after another collection");
            assert_eq!(crate::array::len(array, vm.gc_heap()), 512);
            assert!(crate::array::get(array, vm.gc_heap(), 511).is_undefined());
            assert!(!crate::array::has_own_element(array, vm.gc_heap(), 511));
        });
    }
}

#[test]
fn catchable_iterator_return_throw_preserves_the_exact_original_rejection_once() {
    let mut vm = Interpreter::new().expect("close bootstrap");
    let context = vm
        .link_module(
            crate::test_support::minimal_bytecode_module("fromAsync-close"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("actual context");
    let mut stack = ActivationStack::new();
    let observed = Arc::new(AtomicI32::new(0));
    let closes = Arc::new(AtomicUsize::new(0));
    vm.with_handle_scope(|vm, scope| {
        let state = vm.scoped_object_bare(scope).unwrap();
        let iterator = vm.scoped_object_bare(scope).unwrap();
        let reason = vm.scoped_object_bare(scope).unwrap();
        put(vm, reason, "marker", Value::number_i32(997));
        let callback = crate::native_function::NativeFunction::new(vm.gc_heap_mut(), "return", {
            let closes = Arc::clone(&closes);
            move |_, _, _| {
                closes.fetch_add(1, Ordering::Relaxed);
                Err(NativeError::TypeError {
                    name: "return",
                    reason: "close failure".into(),
                })
            }
        })
        .unwrap();
        put(vm, iterator, "return", Value::native_function(callback));
        let reject = crate::native_function::NativeFunction::new(vm.gc_heap_mut(), "reject", {
            let observed = Arc::clone(&observed);
            move |ctx, args, _| {
                let reason = args
                    .first()
                    .copied()
                    .and_then(Value::as_object)
                    .ok_or_else(|| NativeError::TypeError {
                        name: "reject",
                        reason: "missing reason".into(),
                    })?;
                let marker = crate::object::get_own(reason, ctx.heap(), "marker")
                    .and_then(Value::as_i32)
                    .ok_or_else(|| NativeError::TypeError {
                        name: "reject",
                        reason: "missing marker".into(),
                    })?;
                observed.store(marker, Ordering::Relaxed);
                Ok(Value::undefined())
            }
        })
        .unwrap();
        let iterator_value = vm.escape_scoped(iterator);
        put(vm, state, slot::ITERATOR, iterator_value);
        put(vm, state, slot::REJECT, Value::native_function(reject));
        let st = vm.push_iteration_anchor(vm.escape_scoped(state)) - 1;
        vm.set_pending_uncaught_throw(vm.escape_scoped(reason));
        let error = vm.err_uncaught("original failure".into());
        vm.with_runtime_turn(&mut stack, |turn| {
            let (vm, stack) = turn.into_parts();
            vm.collect_settle_error(
                stack,
                &context,
                st,
                crate::CommittedValueError::JavaScript(error),
            )
            .expect("original rejection settles");
            vm.collect_close_iterator(stack, &context, st, vm.escape_scoped(reason))
                .expect("repeat close no-op");
        });
        assert_eq!(closes.load(Ordering::Relaxed), 1);
        assert_eq!(observed.load(Ordering::Relaxed), 997);
        assert!(
            vm.state_get(vm.escape_scoped(state), slot::ITERATOR)
                .is_undefined()
        );
        vm.pop_iteration_anchors_to(st);
    });
}

#[test]
fn actual_close_allocation_refusal_preserves_requested_bytes_and_cap() {
    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("close cap bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    let context = vm
        .link_module(
            crate::test_support::minimal_bytecode_module("fromAsync-close-oom"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("actual context");
    let mut stack = ActivationStack::new();
    let calls = Arc::new(AtomicUsize::new(0));
    vm.with_handle_scope(|vm, scope| {
        let state = vm.scoped_object_bare(scope).unwrap();
        let iterator = vm.scoped_object_bare(scope).unwrap();
        let reason = vm.scoped_object_bare(scope).unwrap();
        put(vm, reason, "marker", Value::number_i32(719));
        let close = crate::native_function::NativeFunction::new(vm.gc_heap_mut(), "return", {
            let calls = Arc::clone(&calls);
            move |ctx, _, _| {
                calls.fetch_add(1, Ordering::Relaxed);
                ctx.scope(|mut scope| {
                    let _ = scope.string(&"x".repeat(16384))?;
                    Ok(Value::undefined())
                })
            }
        })
        .unwrap();
        put(vm, iterator, "return", Value::native_function(close));
        let iterator_value = vm.escape_scoped(iterator);
        put(vm, state, slot::ITERATOR, iterator_value);
        let st = vm.push_iteration_anchor(vm.escape_scoped(state)) - 1;
        vm.force_gc().expect("settle live close inputs");
        let reserve = cap - vm.gc_heap.stats().allocated_bytes as u64;
        vm.gc_heap
            .reserve_bytes_with_roots(reserve, &mut |_| {})
            .expect("full live admission");
        let before = vm.gc_heap.gc_cycle_counts();
        let result = vm.with_runtime_turn(&mut stack, |turn| {
            let (vm, stack) = turn.into_parts();
            vm.collect_close_iterator(stack, &context, st, vm.escape_scoped(reason))
        });
        vm.gc_heap.release_bytes(reserve);
        assert!(
            vm.gc_heap.gc_cycle_counts().1 > before.1,
            "actual refused allocation full GC"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        match result.expect_err("allocator failure cannot become a close no-op") {
            crate::CommittedValueError::Fatal(VmError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            }) => {
                assert!(requested_bytes > 0);
                assert_eq!(heap_limit_bytes, cap);
            }
            error => panic!("expected actual close OOM, got {error:?}"),
        }
        assert_eq!(
            crate::object::get_own(
                vm.escape_scoped(reason).as_object().unwrap(),
                vm.gc_heap(),
                "marker"
            ),
            Some(Value::number_i32(719))
        );
        assert!(
            vm.state_get(vm.escape_scoped(state), slot::ITERATOR)
                .is_undefined()
        );
        vm.pop_iteration_anchors_to(st);
        assert_eq!(vm.gc_heap.stats().reserved_bytes, 0);
    });
}

#[test]
fn the_scoped_array_length_kernel_moves_its_already_born_receiver() {
    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("born-array bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    vm.force_gc()
        .expect("settle bootstrap before the young receiver is born");
    vm.with_handle_scope(|vm, scope| {
        let setup = vm.gc_heap.gc_cycle_counts();
        // This is the same canonical arena owner and production length kernel
        // used by scoped_array. Capture an existing young body before pressure;
        // a collection before this body's birth cannot satisfy the proof.
        let array = vm.scoped_array(scope, 0).expect("born empty scoped array");
        let value = vm.escape_scoped(array);
        let alias = vm.scoped_value(scope, value);
        let before_array = value.as_array().unwrap().offset();
        let before_funding = vm.gc_heap.stats().allocated_bytes as u64;
        let funding_cell_bytes = otter_gc::page::align_up(
            std::mem::size_of::<otter_gc::GcHeader>() + std::mem::size_of::<Funding>(),
            otter_gc::page::CELL_SIZE,
        ) as u64;
        let slab_bytes = otter_gc::page::align_up(
            std::mem::size_of::<otter_gc::GcHeader>()
                + std::mem::size_of::<crate::array::elements::ElementSlabBody>()
                + crate::array::elements::ElementSlabBody::trailing_bytes(
                    16384,
                    crate::array::elements::DenseElementKind::HoleyDouble,
                ),
            otter_gc::page::CELL_SIZE,
        ) as u64;
        assert_eq!(funding_cell_bytes, 131080);
        assert_eq!(slab_bytes, 133152);
        assert!(
            funding_cell_bytes < slab_bytes,
            "one dead body cannot fund the numeric bitmap/header"
        );
        for _ in 0..2 {
            vm.gc_heap
                .alloc_old(Funding([0; 16384]))
                .expect("dead length funding");
        }
        assert_eq!(
            vm.gc_heap.stats().allocated_bytes as u64,
            before_funding + 2 * funding_cell_bytes
        );
        assert!(2 * funding_cell_bytes >= slab_bytes);
        assert_eq!(vm.gc_heap.gc_cycle_counts(), setup);
        let reserve = cap - vm.gc_heap.tracked_bytes();
        vm.gc_heap
            .reserve_bytes_no_collect(reserve)
            .expect("book cap after array birth");
        assert_eq!(vm.gc_heap.gc_cycle_counts(), setup);
        let before = vm.gc_heap.gc_cycle_counts();
        assert_eq!(
            vm.gc_heap.tracked_bytes(),
            cap,
            "canonical effective usage includes the reservation and refunds unused LAB tail"
        );
        // Numeric holes require data plus a bitmap and both headers (133152B).
        // Effective headroom is zero, so tail retirement alone cannot admit it;
        // the observed operation must reclaim the two dead funding cells.
        let current = vm.escape_scoped(array).as_array().unwrap();
        let growth = crate::array::set_length(current, vm.gc_heap_mut(), 16384);
        vm.gc_heap.release_bytes(reserve);
        growth.expect("collecting production length kernel");
        assert!(vm.gc_heap.gc_cycle_counts().1 > before.1);
        assert_eq!(
            vm.gc_heap.tracked_bytes(),
            vm.gc_heap.stats().allocated_bytes as u64
        );
        assert_eq!(vm.gc_heap.stats().reserved_bytes, 0);
        let current = vm.escape_scoped(array).as_array().unwrap();
        assert_ne!(
            current.offset(),
            before_array,
            "exact already-born receiver relocation during length growth"
        );
        assert_eq!(vm.escape_scoped(array), vm.escape_scoped(alias));
        assert_eq!(crate::array::len(current, vm.gc_heap()), 16384);
        assert!(crate::array::get(current, vm.gc_heap(), 16383).is_undefined());
        assert!(!crate::array::has_own_element(current, vm.gc_heap(), 16383));
        vm.force_gc().expect("retain grown scoped array and alias");
        let current = vm.escape_scoped(array).as_array().unwrap();
        assert_eq!(vm.escape_scoped(array), vm.escape_scoped(alias));
        assert_eq!(crate::array::len(current, vm.gc_heap()), 16384);
        assert!(!crate::array::has_own_element(current, vm.gc_heap(), 16383));
        assert_eq!(vm.gc_heap.stats().reserved_bytes, 0);
    });
}

mod close_provenance;
