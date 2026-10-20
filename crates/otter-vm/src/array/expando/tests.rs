//! Array-to-bag and bag-to-child generational edge proofs.
//!
//! # Contents
//! - Real nursery motion with only the outer Array handle retained.
//! - Named and symbol values surviving writes into a separately tenured bag.
//! - Canonical key partition and present-index enumeration.
//!
//! # Invariants
//! - Inner bag, key and child handles are dropped before each collection.
//! - Numeric offsets are observations only and never become GC roots.
//! - Moving edges are read back through the current managed property owners.
//!
//! # See also
//! - [`super`] projects the managed bag without replacing the Array receiver.
//! - [`crate::handles`] owns scoped allocation and publication.

use super::*;
use crate::{Interpreter, Value};

#[test]
fn key_partition_is_canonical_and_indices_include_accessors_without_holes() {
    for name in [
        "00",
        "01",
        "-0",
        "1.0",
        "4294967295",
        "index",
        "input",
        "groups",
    ] {
        assert_eq!(
            classify_property_key(&VmPropertyKey::String(name)),
            ArrayPropertyKey::Expando
        );
    }
    assert_eq!(
        classify_property_key(&VmPropertyKey::String("length")),
        ArrayPropertyKey::Length
    );
    assert_eq!(
        classify_property_key(&VmPropertyKey::String("4294967294")),
        ArrayPropertyKey::Index(4294967294)
    );
    let mut vm = Interpreter::new().unwrap();
    vm.gc_heap_mut().set_gc_stress(0, false);
    vm.with_handle_scope(|vm, scope| {
        let array = vm.scoped_array(scope, 8).unwrap();
        let value = vm.scoped_value(scope, Value::boolean(true));
        vm.scoped_set_index(scope, array, 2, value).unwrap();
        let current = vm.escape_scoped(array).as_array().unwrap();
        crate::array::set_index_accessor(current, vm.gc_heap_mut(), 5, None, None).unwrap();
        let current = vm.escape_scoped(array).as_array().unwrap();
        assert_eq!(own_indices(current, vm.gc_heap()), [2, 5]);
        assert!(expando_object(current, vm.gc_heap()).is_none());
        assert!(
            vm.gc_heap()
                .read_payload(current, |body| body.dense_own_guard.get() != 0)
        );
    });
}

#[test]
fn only_array_root_retains_young_bag_then_young_named_and_symbol_values() {
    let mut vm = Interpreter::new().unwrap();
    vm.gc_heap_mut().set_gc_stress(0, false);
    vm.with_handle_scope(|vm, outer| {
        let array = vm.scoped_array(outer, 0).unwrap();
        vm.force_gc().unwrap();
        vm.force_gc().unwrap();
        let old_array = vm.escape_scoped(array).as_array().unwrap().offset();
        let fresh_bag = vm.with_handle_scope(|vm, inner| {
            let bag = vm.ensure_array_expando(inner, array).unwrap();
            vm.escape_scoped(bag).as_object().unwrap().offset()
        });
        let before = vm.gc_heap_mut().gc_stats().clone();
        vm.collect_minor_tracing_runtime_roots();
        let after = vm.gc_heap_mut().gc_stats().clone();
        assert!(after.minor_gc_cycles > before.minor_gc_cycles);
        assert!(after.minor_slot_updates > before.minor_slot_updates);
        let current = vm.escape_scoped(array).as_array().unwrap();
        assert_eq!(current.offset(), old_array, "Array is already tenured");
        let bag = expando_object(current, vm.gc_heap()).unwrap();
        assert_ne!(
            bag.offset(),
            fresh_bag,
            "only the Array edge roots the moving bag"
        );
        assert!(crate::object::prototype_value(bag, vm.gc_heap()).is_none());
        assert!(
            vm.gc_heap()
                .read_payload(current, |body| body.exotic.is_null())
        );

        vm.force_gc().unwrap();
        vm.force_gc().unwrap();
        let current = vm.escape_scoped(array).as_array().unwrap();
        let old_bag = expando_object(current, vm.gc_heap()).unwrap().offset();
        let children = vm.with_handle_scope(|vm, inner| {
            let named = vm.scoped_object(inner).unwrap();
            let symbolic = vm.scoped_object(inner).unwrap();
            let marker = vm.scoped_value(inner, Value::number_i32(731));
            vm.scoped_set(inner, named, "marker", marker).unwrap();
            vm.scoped_set(inner, symbolic, "marker", marker).unwrap();
            let description = vm.scoped_string(inner, "expando-key").unwrap();
            let description = vm
                .escape_scoped(description)
                .as_string(vm.gc_heap())
                .unwrap();
            let symbol = crate::symbol::JsSymbol::new(vm.gc_heap_mut(), Some(description)).unwrap();
            let key = vm.scoped_value(inner, Value::symbol(symbol));
            vm.scoped_set(inner, array, "child", named).unwrap();
            let symbol = vm.escape_scoped(key).as_symbol(vm.gc_heap()).unwrap();
            vm.scoped_set_symbol(inner, array, symbol, symbolic)
                .unwrap();
            [
                vm.escape_scoped(named).as_object().unwrap().offset(),
                vm.escape_scoped(symbolic).as_object().unwrap().offset(),
            ]
        });
        let before = vm.gc_heap_mut().gc_stats().clone();
        vm.collect_minor_tracing_runtime_roots();
        let after = vm.gc_heap_mut().gc_stats().clone();
        assert!(after.minor_gc_cycles > before.minor_gc_cycles);
        assert!(after.minor_slot_updates > before.minor_slot_updates);
        let current = vm.escape_scoped(array).as_array().unwrap();
        let bag = expando_object(current, vm.gc_heap()).unwrap();
        assert_eq!(bag.offset(), old_bag, "bag is separately tenured");
        let named = crate::object::get(bag, vm.gc_heap(), "child")
            .unwrap()
            .as_object()
            .unwrap();
        let keys = crate::object::own_symbol_key_values(bag, vm.gc_heap());
        assert_eq!(keys.len(), 1);
        let symbolic = crate::object::get_own_symbol(
            bag,
            vm.gc_heap(),
            keys[0].as_symbol(vm.gc_heap()).unwrap(),
        )
        .unwrap()
        .as_object()
        .unwrap();
        for (child, old) in [(named, children[0]), (symbolic, children[1])] {
            assert_ne!(
                child.offset(),
                old,
                "old bag barrier retains and rewrites the young child"
            );
            assert_eq!(
                crate::object::get(child, vm.gc_heap(), "marker"),
                Some(Value::number_i32(731))
            );
        }
        assert_eq!(
            keys[0]
                .as_symbol(vm.gc_heap())
                .unwrap()
                .description()
                .unwrap()
                .to_lossy_string(vm.gc_heap()),
            "expando-key"
        );
        assert!(
            vm.gc_heap()
                .read_payload(current, |body| body.exotic.is_null())
        );
    });
}

#[test]
fn snapshot_relocates_managed_bag_and_rebuilds_only_numeric_indexed_records() {
    use crate::{NativeCallInfo, NativeCtx, NativeError};
    let mut source = Interpreter::new().unwrap();
    source.gc_heap_mut().set_gc_stress(0, false);
    NativeCtx::with_host_context(&mut source, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let array = scope.array(1)?;
            let child = scope.string("retained bag child")?;
            scope.set_index(array, 0, child)?;
            scope.define(
                array,
                "input",
                child,
                crate::object::PropertyFlags::new(false, false, true),
            )?;
            let description = scope.string("snapshot-symbol")?;
            let description = scope
                .raw(description)
                .as_string(scope.context().heap())
                .unwrap();
            let symbol =
                crate::symbol::JsSymbol::new(scope.context().heap_mut(), Some(description))?;
            let symbol = scope.value(Value::symbol(symbol));
            scope.define_symbol(
                array,
                symbol,
                child,
                crate::object::PropertyFlags::new(true, false, true),
            )?;
            scope.with_turn_parts(|vm, _| {
                let current = vm.escape_scoped(array).as_array().unwrap();
                crate::array::set_index_flags(
                    current,
                    vm.gc_heap_mut(),
                    0,
                    crate::object::PropertyFlags::new(false, true, false),
                )
                .unwrap();
                let current = vm.escape_scoped(array).as_array().unwrap();
                crate::array::set_length_writable(current, vm.gc_heap_mut(), false).unwrap();
            });
            let global = scope.global_this();
            scope.set(global, "snapshotArrayExpando", array)?;
            Ok::<_, NativeError>(())
        })
    })
    .unwrap();
    // The full collector starts with one age-based scavenge. A fresh
    // survivor needs another rooted scavenge before an old-space image.
    let minors_before = source.gc_heap_mut().gc_stats().minor_gc_cycles;
    source.collect_minor_tracing_runtime_roots();
    source.force_gc().unwrap();
    assert!(source.gc_heap_mut().gc_stats().minor_gc_cycles >= minors_before + 2);
    assert_eq!(source.gc_heap().census().young.object_count, 0);
    let image = source.capture_isolate_snapshot().unwrap();
    assert!(image.array_indexed_records.iter().any(|(_, record)| {
        !record.length_writable
            && record
                .property_flags
                .iter()
                .any(|&(index, flags)| index == 0 && !flags.writable())
    }));
    let mut restored =
        Interpreter::from_isolate_snapshot(&image, &otter_resource::ResourceAccount::default())
            .unwrap();
    restored.force_gc().unwrap();
    NativeCtx::with_host_context(&mut restored, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let array = scope
                .global("snapshotArrayExpando")
                .expect("restored Array root");
            scope.with_turn_parts(|vm, _| {
                let current = vm.escape_scoped(array).as_array().unwrap();
                assert!(!crate::array::length_writable(current, vm.gc_heap()));
                assert!(
                    !crate::array::get_index_flags(current, vm.gc_heap(), 0)
                        .unwrap()
                        .writable()
                );
                let bag = expando_object(current, vm.gc_heap()).unwrap();
                assert!(crate::object::prototype_value(bag, vm.gc_heap()).is_none());
                let descriptor =
                    crate::object::get_own_descriptor(bag, vm.gc_heap(), "input").unwrap();
                assert!(
                    !descriptor.writable() && !descriptor.enumerable() && descriptor.configurable()
                );
                let child = crate::object::get(bag, vm.gc_heap(), "input").unwrap();
                assert_eq!(
                    child
                        .as_string(vm.gc_heap())
                        .unwrap()
                        .to_lossy_string(vm.gc_heap()),
                    "retained bag child"
                );
                assert_eq!(crate::array::get(current, vm.gc_heap(), 0), child);
                let symbols = crate::object::own_symbol_key_values(bag, vm.gc_heap());
                assert_eq!(symbols.len(), 1);
                let key = symbols[0].as_symbol(vm.gc_heap()).unwrap();
                assert_eq!(
                    key.description().unwrap().to_lossy_string(vm.gc_heap()),
                    "snapshot-symbol"
                );
                assert_eq!(
                    crate::object::get_own_symbol(bag, vm.gc_heap(), key),
                    Some(child)
                );
                assert_eq!(own_indices(current, vm.gc_heap()), [0]);
            });
            Ok::<_, NativeError>(())
        })
    })
    .unwrap();
}

#[test]
fn indexed_state_allocation_refusal_is_fallible_and_publishes_no_sidecar() {
    for operation in 0..4 {
        let cap = 4 * 1024 * 1024;
        let mut vm = Interpreter::with_string_heap_cap(cap).unwrap();
        vm.gc_heap_mut().set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let array = vm.scoped_array(scope, 0).unwrap();
            vm.force_gc().unwrap();
            let reserve = cap - vm.gc_heap().tracked_bytes() - 1;
            vm.gc_heap_mut().reserve_bytes_no_collect(reserve).unwrap();
            let before = vm.gc_heap().gc_cycle_counts();
            let current = vm.escape_scoped(array).as_array().unwrap();
            let result = match operation {
                0 => crate::array::set_index_flags(
                    current,
                    vm.gc_heap_mut(),
                    0,
                    crate::object::PropertyFlags::new(false, true, true),
                ),
                1 => crate::array::set_index_accessor(current, vm.gc_heap_mut(), 0, None, None),
                2 => crate::array::set_length_writable(current, vm.gc_heap_mut(), false),
                3 => crate::array::prevent_extensions(current, vm.gc_heap_mut()),
                _ => unreachable!(),
            };
            let error = result.expect_err("the actual cap cannot admit an indexed sidecar");
            vm.gc_heap_mut().release_bytes(reserve);
            assert!(error.requested_bytes() > 0);
            assert_eq!(error.heap_limit_bytes(), cap);
            assert!(
                vm.gc_heap().gc_cycle_counts().1 > before.1,
                "allocation refusal follows actual collection"
            );
            let current = vm.escape_scoped(array).as_array().unwrap();
            assert!(
                vm.gc_heap()
                    .read_payload(current, |body| body.exotic.is_null()
                        && body.expando.is_null())
            );
            assert_eq!(crate::array::len(current, vm.gc_heap()), 0);
            assert!(crate::array::length_writable(current, vm.gc_heap()));
            assert!(crate::array::is_extensible(current, vm.gc_heap()));
        });
    }
}
