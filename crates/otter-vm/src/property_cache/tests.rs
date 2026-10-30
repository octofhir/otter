//! Shared action coexistence, real collection and matched-refusal proofs.
//!
//! # Contents
//! - An inherited writable load and own append keep different slots under one key.
//! - Full GC clears weak loads while retaining the actual append target word.
//! - A matched allocating recipe preserves its real OOM and moving input roots.
//! - Fixed table and proof-owner payloads stay within the replaced cache budget.
//! - Numeric words and nonnumbers reuse the same immutable slot geometry.
//!
//! # Invariants
//! Fixtures use the production interpreter handle arena and collection roots.
//! Shape/descriptor mutation uses actual owners; no cache entry is fabricated.
//! Every motion claim compares exact cell offsets, and refusal never commits.
//!
//! # See also
//! - `super` owns the sole key table and independent action publication.

use super::*;
use crate::property_atom::PropertyAtom;
use crate::{Interpreter, Value};

/// The live data slot a load of `key` on `obj` resolves, if any.
fn resolved_data_slot(
    vm: &Interpreter,
    obj: JsObject,
    key: AtomizedPropertyKey<'_>,
) -> Option<cache_ir::ResolvedDataSlot> {
    match vm.resolve_property_load(obj, key) {
        PropertyLoad::Data(resolved) => Some(resolved),
        PropertyLoad::Absent(_) | PropertyLoad::Other => None,
    }
}

fn key<'a>(vm: &mut Interpreter, name: &'a str) -> AtomizedPropertyKey<'a> {
    AtomizedPropertyKey::new(
        PropertyAtom::new(crate::isolate_names::names_mut(&mut vm.gc_heap).intern(name)),
        name,
    )
}

#[test]
fn shared_store_keeps_geometry_and_numeric_summary_across_value_categories() {
    let mut vm = Interpreter::new().unwrap();
    vm.with_handle_scope(|vm, scope| {
        let rooted = vm.scoped_object_bare(scope).unwrap();
        let mut receiver = vm.escape_scoped(rooted).as_object().unwrap();
        vm.create_data_property(&mut receiver, "x", Value::number_i32(0))
            .unwrap();
        let property = key(vm, "x");
        let shape = object::shape(receiver, &vm.gc_heap);
        resolved_data_slot(vm, receiver, property).unwrap();
        for physical in [Value::double(0.0), Value::double(1.25), Value::TRUE] {
            assert_eq!(
                vm.property_cache
                    .replay_store(receiver, &mut vm.gc_heap, property, &physical)
                    .unwrap(),
                Some(PropertyStoreAction::OwnWritable)
            );
            assert_eq!(object::shape(receiver, &vm.gc_heap), shape);
            let resolved = resolved_data_slot(vm, receiver, property).unwrap();
            assert_eq!(resolved.value, physical);
        }
        assert_eq!(
            object::field_type::snapshot(object::field_type::owner(shape, 0).unwrap()),
            object::field_type::TOP
        );
    });
}

#[test]
fn inherited_numeric_load_and_own_append_keep_independent_values() {
    vm_numeric_actions(|vm, mut first, second, property| {
        let resolved = resolved_data_slot(vm, second, property).unwrap();
        assert_eq!(resolved.hops, 1);
        let source = object::shape(second, &vm.gc_heap);
        let child = vm.shape_child(source, property.name()).unwrap();
        let mut value = Value::TRUE;
        let transition = object::capture_store_property_transition_with_shape(
            &mut first,
            &mut vm.gc_heap,
            property,
            &mut value,
            child,
        )
        .expect("own append preparation")
        .expect("actual own tagged append over inherited double");
        vm.property_cache.record_store(transition);
        let index = vm
            .property_cache
            .find(object::shape_body::id_of(source), property.atom().id())
            .unwrap();
        let entry = vm.property_cache.entries[index].get();
        assert_eq!(entry.load_slot, resolved.hit.slot);
        assert_eq!(entry.store_slot, 0);
        let PropertyProbe::Resolved(load) =
            vm.property_cache.probe_load(second, &vm.gc_heap, property)
        else {
            panic!("independent inherited load");
        };
        assert_eq!(load.value, Value::number_f64(1.25));
        assert!(
            vm.property_cache
                .replay_store(second, &mut vm.gc_heap, property, &Value::FALSE)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            object::get_own(second, &vm.gc_heap, property.name()),
            Some(Value::FALSE)
        );
    });
}

fn vm_numeric_actions(
    test: impl FnOnce(&mut Interpreter, JsObject, JsObject, AtomizedPropertyKey<'_>),
) {
    let mut vm = Interpreter::new().expect("fixture interpreter");
    vm.with_handle_scope(|vm, scope| {
        let mut prototype = object::alloc_object_old_for_fixture(&mut vm.gc_heap).unwrap();
        let prototype_root = vm.scoped_value(scope, Value::object(prototype));
        vm.create_data_property(&mut prototype, "independentPolicy", Value::number_f64(1.25))
            .unwrap();
        let mut first = object::alloc_object_old_for_fixture(&mut vm.gc_heap).unwrap();
        let first_root = vm.scoped_value(scope, Value::object(first));
        object::set_prototype(&mut first, &mut vm.gc_heap, Some(prototype)).unwrap();
        let mut second = object::alloc_object_old_for_fixture(&mut vm.gc_heap).unwrap();
        let second_root = vm.scoped_value(scope, Value::object(second));
        let prototype = vm.escape_scoped(prototype_root).as_object().unwrap();
        object::set_prototype(&mut second, &mut vm.gc_heap, Some(prototype)).unwrap();
        let property = key(vm, "independentPolicy");
        let first = vm.escape_scoped(first_root).as_object().unwrap();
        let second = vm.escape_scoped(second_root).as_object().unwrap();
        test(vm, first, second, property);
    });
}

#[test]
fn inherited_load_and_own_append_preserve_independent_slots_and_writability() {
    let mut vm = Interpreter::new().expect("fixture interpreter");
    vm.with_handle_scope(|vm, scope| {
        let mut prototype = object::alloc_object_old_for_fixture(&mut vm.gc_heap).unwrap();
        let prototype_root = vm.scoped_value(scope, Value::object(prototype));
        vm.create_data_property(&mut prototype, "padding", Value::number_i32(31))
            .unwrap();
        vm.create_data_property(&mut prototype, "shared", Value::number_i32(53))
            .unwrap();
        let mut first = object::alloc_object_old_for_fixture(&mut vm.gc_heap).unwrap();
        let first_root = vm.scoped_value(scope, Value::object(first));
        assert!(object::set_prototype(&mut first, &mut vm.gc_heap, Some(prototype)).unwrap());
        let mut second = object::alloc_object_old_for_fixture(&mut vm.gc_heap).unwrap();
        let second_root = vm.scoped_value(scope, Value::object(second));
        assert!(object::set_prototype(&mut second, &mut vm.gc_heap, Some(prototype)).unwrap());
        let mut third = object::alloc_object_old_for_fixture(&mut vm.gc_heap).unwrap();
        let third_root = vm.scoped_value(scope, Value::object(third));
        assert!(object::set_prototype(&mut third, &mut vm.gc_heap, Some(prototype)).unwrap());
        let key = key(vm, "shared");
        assert!(matches!(
            vm.property_cache.probe_load(second, &vm.gc_heap, key),
            PropertyProbe::Unknown
        ));
        let resolved = resolved_data_slot(vm, second, key).unwrap();
        assert_eq!(resolved.hops, 1);
        assert_eq!(resolved.hit.slot, 1);
        assert!(resolved.is_writable, "actual inherited descriptor");
        let PropertyProbe::Resolved(before_store) =
            vm.property_cache.probe_load(second, &vm.gc_heap, key)
        else {
            panic!("published load fact");
        };
        assert!(
            before_store.is_writable,
            "writable even before any store fact"
        );
        assert_eq!(before_store.value, Value::number_i32(53));
        let source = object::shape(second, &vm.gc_heap);
        let child = vm.shape_child(source, key.name()).unwrap();
        let mut value = Value::number_i32(71);
        let transition = object::capture_store_property_transition_with_shape(
            &mut first,
            &mut vm.gc_heap,
            key,
            &mut value,
            child,
        )
        .expect("writable append preparation")
        .expect("actual direct-prototype writable append");
        assert!(matches!(
            transition.kind,
            object::StorePropertyTransitionKind::PrototypeWritableData { .. }
        ));
        assert_eq!(
            transition.slot, 0,
            "receiver append differs from holder slot"
        );
        vm.property_cache.record_store(transition);
        let PropertyProbe::Resolved(after_store) =
            vm.property_cache.probe_load(second, &vm.gc_heap, key)
        else {
            panic!("append publication must preserve the inherited load");
        };
        assert_eq!(after_store.hit.slot, 1);
        assert!(after_store.is_writable);
        assert_eq!(after_store.value, Value::number_i32(53));
        assert_eq!(
            vm.property_cache
                .replay_store(second, &mut vm.gc_heap, key, &Value::number_i32(89))
                .unwrap(),
            Some(PropertyStoreAction::AddOwn)
        );
        second = vm.escape_scoped(second_root).as_object().unwrap();
        assert_eq!(
            object::get_own(second, &vm.gc_heap, "shared"),
            Some(Value::number_i32(89))
        );
        assert_eq!(
            object::get_own(prototype, &vm.gc_heap, "shared"),
            Some(Value::number_i32(53))
        );
        let (_, own) =
            crate::property_ic::IcHandler::store_existing_hit(second, &vm.gc_heap, key).unwrap();
        vm.property_cache.record_own_store(own);
        assert_eq!(
            vm.property_cache
                .replay_store(second, &mut vm.gc_heap, key, &Value::number_i32(97))
                .unwrap(),
            Some(PropertyStoreAction::OwnWritable)
        );
        assert_eq!(
            object::get_own(second, &vm.gc_heap, "shared"),
            Some(Value::number_i32(97))
        );
        assert_eq!(
            object::shape(second, &vm.gc_heap),
            child,
            "overwrite adds no property"
        );
        // Mutation invalidates both original chain-dependent actions before effects.
        assert!(
            object::define_own_property_partial(
                &mut prototype,
                &mut vm.gc_heap,
                "shared",
                object::PartialPropertyDescriptor {
                    writable: Some(false),
                    ..Default::default()
                },
            )
            .unwrap()
        );
        // The prototype now owns a different descriptor shape. Full GC must
        // clear the old weak holder fact without dropping the independent
        // retained append recipe, whose nonreviving proof remains invalid.
        let old_holder_id = after_store.hit.shape_id;
        vm.shape_runtime.unpin_turn_shapes(&vm.gc_heap);
        vm.gc_heap
            .collect_full(&mut |_| {})
            .expect("actual descriptor-retirement collection");
        assert_eq!(
            vm.shape_runtime.handle_for_id(&vm.gc_heap, old_holder_id),
            None
        );
        let original_way = vm
            .property_cache
            .find(
                vm.gc_heap
                    .read_payload(source, object::shape_body::ShapeBody::id),
                key.atom().id(),
            )
            .expect("independent store fact survives weak load clearing");
        let original = vm.property_cache.entries[original_way].get();
        assert_eq!(original.load_action, PropertyLoadAction::Unknown);
        assert_eq!(original.store_action, PropertyStoreAction::AddOwn);
        assert_eq!(original.target_shape, child);
        third = vm.escape_scoped(third_root).as_object().unwrap();
        assert!(matches!(
            vm.property_cache.probe_load(third, &vm.gc_heap, key),
            PropertyProbe::Unknown
        ));
        assert_eq!(
            vm.property_cache
                .replay_store(third, &mut vm.gc_heap, key, &Value::null())
                .unwrap(),
            None
        );
        assert_eq!(object::get_own(third, &vm.gc_heap, "shared"), None);
        assert_eq!(
            object::get(third, &vm.gc_heap, "shared"),
            Some(Value::number_i32(53))
        );
        assert_eq!(vm.escape_scoped(first_root).as_object().unwrap(), first);
        assert_eq!(
            vm.escape_scoped(prototype_root).as_object().unwrap(),
            prototype
        );
    });
}

#[test]
fn full_gc_clears_weak_loads_and_keeps_the_actual_append_word_live() {
    let mut vm = Interpreter::new().expect("fixture interpreter");
    vm.gc_heap.set_gc_stress(0, false);
    vm.with_handle_scope(|vm, scope| {
        let second = object::alloc_object_old_for_fixture(&mut vm.gc_heap).unwrap();
        let second = vm.scoped_value(scope, Value::object(second));
        let append_key = key(vm, "retainedAppend");
        let (target_id, target) = vm.with_handle_scope(|vm, inner| {
            let first = object::alloc_object_old_for_fixture(&mut vm.gc_heap).unwrap();
            let first = vm.scoped_value(inner, Value::object(first));
            let mut first = vm.escape_scoped(first).as_object().unwrap();
            let child = vm
                .shape_child(object::shape(first, &vm.gc_heap), append_key.name())
                .unwrap();
            let mut value = Value::TRUE;
            let transition = object::capture_store_property_transition_with_shape(
                &mut first,
                &mut vm.gc_heap,
                append_key,
                &mut value,
                child,
            )
            .unwrap()
            .unwrap();
            let target_id = transition.to_shape_id;
            vm.property_cache.record_store(transition);
            (target_id, child)
        });
        let weak_key = key(vm, "deadInherited");
        let (weak_receiver, weak_holder) = vm.with_handle_scope(|vm, inner| {
            let mut holder = object::alloc_object_old_for_fixture(&mut vm.gc_heap).unwrap();
            let holder_root = vm.scoped_value(inner, Value::object(holder));
            vm.create_data_property(&mut holder, weak_key.name(), Value::number_i32(113))
                .unwrap();
            let mut receiver = object::alloc_object_old_for_fixture(&mut vm.gc_heap).unwrap();
            let receiver_root = vm.scoped_value(inner, Value::object(receiver));
            assert!(object::set_prototype(&mut receiver, &mut vm.gc_heap, Some(holder)).unwrap());
            let resolved = resolved_data_slot(vm, receiver, weak_key).unwrap();
            assert_eq!(resolved.value, Value::number_i32(113));
            assert_eq!(vm.escape_scoped(holder_root).as_object().unwrap(), holder);
            assert_eq!(
                vm.escape_scoped(receiver_root).as_object().unwrap(),
                receiver
            );
            (
                object::shape_id(receiver, &vm.gc_heap),
                resolved.hit.shape_id,
            )
        });
        assert!(
            vm.property_cache
                .find(weak_receiver, weak_key.atom().id())
                .is_some()
        );
        vm.shape_runtime.unpin_turn_shapes(&vm.gc_heap);
        let before = vm.gc_heap.gc_cycle_counts();
        vm.gc_heap
            .collect_full(&mut |_| {})
            .expect("actual full collection");
        assert!(vm.gc_heap.gc_cycle_counts().1 > before.1);
        assert_eq!(
            vm.shape_runtime.handle_for_id(&vm.gc_heap, weak_holder),
            None,
            "dead holder actually swept"
        );
        assert_eq!(
            vm.property_cache.find(weak_receiver, weak_key.atom().id()),
            None,
            "weak fact removed before reuse"
        );
        assert_eq!(
            vm.shape_runtime.handle_for_id(&vm.gc_heap, target_id),
            Some(target),
            "cache scalar word alone retains append child"
        );
        let payload = vm.scoped_object(scope).expect("fresh young payload");
        let marker = vm.scoped_value(scope, Value::number_i32(719));
        vm.scoped_set(scope, payload, "marker", marker).unwrap();
        let payload_before = vm.escape_scoped(payload).as_object().unwrap().offset();
        let receiver = vm.escape_scoped(second).as_object().unwrap();
        let value = vm.escape_scoped(payload);
        assert_eq!(
            vm.property_cache
                .replay_store(receiver, &mut vm.gc_heap, append_key, &value)
                .unwrap(),
            Some(PropertyStoreAction::AddOwn)
        );
        vm.collect_minor_tracing_runtime_roots();
        let payload = vm.escape_scoped(payload);
        assert_ne!(
            payload.as_object().unwrap().offset(),
            payload_before,
            "exact fresh stored child moves"
        );
        let receiver = vm.escape_scoped(second).as_object().unwrap();
        assert_eq!(
            object::get_own(receiver, &vm.gc_heap, append_key.name()),
            Some(payload)
        );
        assert_eq!(
            object::get_own(payload.as_object().unwrap(), &vm.gc_heap, "marker"),
            Some(Value::number_i32(719))
        );
        assert_eq!(object::shape(receiver, &vm.gc_heap), target);
    });
}

#[test]
fn allocating_shared_replay_preserves_cap_cause_and_does_not_commit_or_hold_proof_borrow() {
    let limit = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(limit).expect("fixture interpreter");
    vm.gc_heap.set_gc_stress(0, false);
    vm.with_handle_scope(|vm, scope| {
        let first = vm.alloc_runtime_rooted_object_with_roots(&mut [], &mut []).unwrap();
        let first = vm.scoped_value(scope, Value::object(first));
        let second = vm.alloc_runtime_rooted_object_with_roots(&mut [], &mut []).unwrap();
        let second = vm.scoped_value(scope, Value::object(second));
        let key = key(vm, "allocatingAppend");
        let transition = object::capture_store_property_transition(
            vm.escape_scoped(first).as_object().unwrap(), &mut vm.gc_heap, key, &Value::boolean(true),
        ).expect("actual allocating dictionary replay sample");
        assert!(transition.to_shape.get().is_null());
        let source = transition.from_shape_id;
        vm.property_cache.record_store(transition);
        let (_, entry) = vm.property_cache.entry_for(
            vm.escape_scoped(second).as_object().unwrap(), &vm.gc_heap, key,
        ).expect("published VM dictionary append");
        assert_eq!(entry.store_action, PropertyStoreAction::AddOwn);
        assert!(entry.target_shape.is_null());
        assert!(entry.store_owner.is_null());
        let mut settled = false;
        vm.shape_runtime.unpin_turn_shapes(&vm.gc_heap);
        for round in 0..8 {
            let before = vm.gc_heap.stats().allocated_bytes;
            vm.gc_heap.collect_full(&mut |_| {}).expect("settle actual live heap");
            if round > 0 && before == vm.gc_heap.stats().allocated_bytes { settled = true; break; }
        }
        assert!(settled, "cap premise uses a stable live set");
        let payload = vm.alloc_runtime_rooted_object_with_roots(&mut [], &mut []).unwrap();
        let payload = vm.scoped_value(scope, Value::object(payload));
        let alias = vm.scoped_value(scope, vm.escape_scoped(payload));
        let payload_before = vm.escape_scoped(payload).as_object().unwrap().offset();
        let reserved = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes();
        vm.gc_heap.reserve_bytes_no_collect(reserved).expect("fill actual cap without setup GC");
        let cycles = vm.gc_heap.gc_cycle_counts();
        let receiver = vm.escape_scoped(second).as_object().unwrap();
        let value = vm.escape_scoped(payload);
        let result = vm.property_cache.replay_store(receiver, &mut vm.gc_heap, key, &value);
        vm.gc_heap.release_bytes(reserved);
        assert!(matches!(result, Err(otter_gc::OutOfMemory::HeapCapExceeded { requested_bytes, heap_limit_bytes }) if requested_bytes > 0 && heap_limit_bytes == limit), "actual matched allocator cause: {result:?}");
        assert!(vm.gc_heap.gc_cycle_counts().1 > cycles.1, "replay allocation itself reaches full GC");
        let receiver = vm.escape_scoped(second).as_object().unwrap();
        assert_eq!(object::shape_id(receiver, &vm.gc_heap), source);
        assert_eq!(object::get_own(receiver, &vm.gc_heap, key.name()), None, "failed matched action commits no prefix");
        assert_ne!(vm.escape_scoped(payload).as_object().unwrap().offset(), payload_before, "actual fresh incoming child moved on refusal");
        assert_eq!(vm.escape_scoped(payload), vm.escape_scoped(alias));
    });
}

#[test]
fn rows_reserve_a_bounded_table_and_start_empty_with_no_owners() {
    let entries = SETS * WAYS;
    let bytes = entries * std::mem::size_of::<Cell<PropertyActionEntry>>();
    // One isolate's reserved rows; the zeroed allocation commits a page only
    // once a row on it is written, and proof owners exist only for filled rows.
    let budget = 8 * 1024 * 1024;
    assert!(
        bytes <= budget,
        "reserved rows {bytes} exceed their {budget}-byte budget"
    );
    let table = PropertyActionCache::default();
    assert!(table.entries.iter().all(|row| {
        let row = row.get();
        row.receiver_shape_id == ShapeId::UNASSIGNED
            && row.load_action == PropertyLoadAction::Unknown
            && row.store_action == PropertyStoreAction::Unknown
            && row.target_shape.is_null()
    }));
    assert!(table.owners.borrow().is_empty());
}
