//! Real collection and typed-failure proofs for immutable object state.
//!
//! # Contents
//! - Capacity 0/4/64 source lineages keep banks, keys and descriptors after state.
//! - Source and partial-result roots survive cap-triggered moving collection.
//! - Failed state preparation preserves the original semantic shape and aliases.
//!
//! # Invariants
//! - Values use the production interpreter handle scope and actual caller slots.
//! - Pressure setup does not collect after fresh nursery children are created.
//! - Every claimed moving child or key compares exact before/after offsets.
//! - OOM is an ordinary typed return; the mutation never panics or bypasses cap.
//!
//! # See also
//! - `super::super::state_transition` owns rooted preparation/publication.

use crate::object::{self, FieldLayout, FieldLocation, ShapeHandle, ShapeState, shape_body};
use crate::{Interpreter, Value};
use otter_gc::SafeTraceable;

struct Reclaimable([u64; 16384]);
impl SafeTraceable for Reclaimable {
    const TYPE_TAG: u8 = 0xe9;
    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        let _ = self.0[0];
    }
}

fn source_shape(vm: &mut Interpreter, capacity: usize) -> (ShapeHandle, usize) {
    let count = capacity.max(4) + 2;
    let mut shape = vm
        .shape_runtime
        .new_root(
            &mut vm.gc_heap,
            shape_body::ShapePrototype::Null,
            capacity,
            ShapeHandle::null(),
            ShapeState::ORDINARY,
            &mut |_| {},
        )
        .expect("source root");
    for index in 0..count {
        shape = vm
            .shape_child(shape, &format!("p{index}"))
            .expect("source child");
    }
    (shape, count)
}

#[test]
fn state_preparation_moves_keys_and_distinct_bank_children_without_changing_source() {
    for capacity in [0, 4, 64] {
        let mut vm =
            Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let (source, count) = source_shape(vm, capacity);
            let object =
                object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {})
                    .expect("source object");
            let owner = vm.scoped_value(scope, Value::object(object));
            let alias = vm.scoped_value(scope, vm.escape_scoped(owner));
            let first = vm.scoped_object(scope).expect("fresh first child");
            let last = vm.scoped_object(scope).expect("fresh last child");
            let first_marker = vm.scoped_value(scope, Value::number_i32(317));
            let last_marker = vm.scoped_value(scope, Value::number_i32(719));
            vm.scoped_set(scope, first, "marker", first_marker)
                .expect("first marker");
            vm.scoped_set(scope, last, "marker", last_marker)
                .expect("last marker");
            vm.scoped_set_slot(scope, owner, 0, first)
                .expect("first bank child");
            vm.scoped_set_slot(scope, owner, capacity.max(4), last)
                .expect("last bank child");
            let child_offsets = [
                vm.escape_scoped(first).as_object().unwrap().offset(),
                vm.escape_scoped(last).as_object().unwrap().offset(),
            ];
            assert_ne!(child_offsets[0], child_offsets[1]);
            let key_offsets: Vec<_> = shape_body::shape_keys_ordered(&vm.gc_heap, source)
                .into_iter()
                .map(|(key, _)| key.offset())
                .collect();
            let locations: Vec<_> = (0..count)
                .map(|index| {
                    object::field_location_at(
                        vm.escape_scoped(owner).as_object().unwrap(),
                        &vm.gc_heap,
                        index as u32,
                    )
                })
                .collect();
            // The only reclaimable old allocation funds this state operation's
            // root/child bodies after the cap admission performs a real full GC.
            let _ = vm
                .gc_heap
                .alloc_old(Reclaimable([0; 16384]))
                .expect("reclaimable pressure");
            let reserved = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes();
            vm.gc_heap
                .reserve_bytes_no_collect(reserved)
                .expect("fill cap without collection");
            let before = vm.gc_heap.gc_cycle_counts();
            let mut current = vm.escape_scoped(owner).as_object().unwrap();
            object::prevent_extensions(&mut current, &mut vm.gc_heap)
                .expect("collecting state preparation");
            vm.gc_heap.release_bytes(reserved);
            assert!(
                vm.gc_heap.gc_cycle_counts().1 > before.1,
                "state allocation itself must full-collect"
            );
            assert_eq!(Value::object(current), vm.escape_scoped(owner));
            assert_eq!(vm.escape_scoped(owner), vm.escape_scoped(alias));
            assert!(!object::is_extensible(current, &vm.gc_heap));
            assert_eq!(
                shape_body::state_of(source),
                ShapeState::ORDINARY,
                "old source state remains immutable"
            );
            assert_ne!(object::shape(current, &vm.gc_heap), source);
            for (index, old_location) in locations.into_iter().enumerate() {
                assert_eq!(
                    object::field_location_at(current, &vm.gc_heap, index as u32),
                    old_location
                );
                let descriptor =
                    object::get_own_descriptor(current, &vm.gc_heap, &format!("p{index}")).unwrap();
                assert!(
                    descriptor.writable() && descriptor.enumerable() && descriptor.configurable()
                );
            }
            let keys = shape_body::shape_keys_ordered(&vm.gc_heap, source);
            for ((key, _), before) in keys.into_iter().zip(key_offsets) {
                assert_ne!(
                    key.offset(),
                    before,
                    "source key actually moved during preparation"
                );
            }
            for (index, (child, before)) in [first, last].into_iter().zip(child_offsets).enumerate()
            {
                let current_child = vm.escape_scoped(child).as_object().unwrap();
                assert_ne!(
                    current_child.offset(),
                    before,
                    "fresh bank child actually relocated"
                );
                let key = if index == 0 {
                    "p0".to_owned()
                } else {
                    format!("p{}", capacity.max(4))
                };
                assert_eq!(
                    object::get_own(current, &vm.gc_heap, &key),
                    Some(Value::object(current_child))
                );
                assert_eq!(
                    object::get_own(current_child, &vm.gc_heap, "marker"),
                    Some(Value::number_i32(if index == 0 { 317 } else { 719 }))
                );
            }
            vm.migrate_slow_to_fast(&mut current);
            let state_shape = object::shape(current, &vm.gc_heap);
            assert_eq!(
                vm.shape_runtime
                    .handle_for_id(shape_body::id_of(state_shape)),
                Some(state_shape)
            );
            vm.gc_heap.read_payload(current, |body| {
                assert_eq!(body.inline_capacity(), capacity);
                // SAFETY: the handle is live in the production scope, and no
                // allocation occurs while the resident header is inspected.
                assert_eq!(unsafe { (*current.as_header_ptr()).body_bytes() }, [0, 0]);
            });
        });
    }
}

#[test]
fn state_oom_returns_updated_aliases_without_publishing_partial_state() {
    for capacity in [0, 4, 64] {
        let mut vm =
            Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let (source, count) = source_shape(vm, capacity);
            // The source owns an existing marker slot in every fresh cell.
            // Preparation is settled before admission; the three young cells
            // require no fresh key, sidecar or descriptor allocation.
            vm.force_gc().expect("settle bootstrap and source setup");
            let geometry = FieldLayout::current();
            let one_input = geometry.cell_bytes(capacity) as u64
                + u64::from(geometry.slab_words_byte)
                + FieldLocation::words_bytes(count - capacity) as u64;
            let input_bytes = 3 * one_input;
            let reserved = vm.gc_heap.max_heap_bytes()
                - vm.gc_heap.stats().allocated_bytes as u64
                - input_bytes
                - 1;
            vm.gc_heap
                .reserve_bytes_with_roots(reserved, &mut |_| {})
                .expect("reconcile cap before fresh inputs");
            let admitted = vm.gc_heap.stats();
            assert_eq!(
                admitted.tracked_bytes,
                admitted.allocated_bytes as u64 + admitted.reserved_bytes
            );
            let cycles = vm.gc_heap.gc_cycle_counts();
            let object =
                object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {})
                    .expect("exact owner footprint");
            let owner = vm.scoped_value(scope, Value::object(object));
            let alias = vm.scoped_value(scope, vm.escape_scoped(owner));
            let first = object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {})
                .expect("fresh first shaped child");
            let first = vm.scoped_value(scope, Value::object(first));
            let last = object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {})
                .expect("fresh last shaped child");
            let last = vm.scoped_value(scope, Value::object(last));
            let marker = vm.scoped_value(scope, Value::number_i32(997));
            vm.scoped_set_slot(scope, first, 0, marker).unwrap();
            vm.scoped_set_slot(scope, last, 0, marker).unwrap();
            vm.scoped_set_slot(scope, owner, 0, first)
                .expect("first bank child");
            vm.scoped_set_slot(scope, owner, capacity.max(4), last)
                .expect("last bank child");
            assert_eq!(
                vm.gc_heap.gc_cycle_counts(),
                cycles,
                "no setup collection ages fresh children"
            );
            assert_eq!(
                vm.gc_heap.stats().allocated_bytes - admitted.allocated_bytes,
                input_bytes as usize
            );
            let child_offsets = [
                vm.escape_scoped(first).as_object().unwrap().offset(),
                vm.escape_scoped(last).as_object().unwrap().offset(),
            ];
            let mut current = vm.escape_scoped(owner).as_object().unwrap();
            let result = object::prevent_extensions(&mut current, &mut vm.gc_heap);
            vm.gc_heap.release_bytes(reserved);
            assert!(
                matches!(result, Err(otter_gc::OutOfMemory::HeapCapExceeded { .. })),
                "{result:?}"
            );
            assert!(vm.gc_heap.gc_cycle_counts().1 > cycles.1);
            assert_eq!(Value::object(current), vm.escape_scoped(owner));
            assert_eq!(vm.escape_scoped(owner), vm.escape_scoped(alias));
            assert_eq!(object::shape(current, &vm.gc_heap), source);
            assert_eq!(object::state(current, &vm.gc_heap), ShapeState::ORDINARY);
            assert!(object::is_extensible(current, &vm.gc_heap));
            for (index, (child, before)) in [first, last].into_iter().zip(child_offsets).enumerate()
            {
                let moved = vm.escape_scoped(child).as_object().unwrap();
                assert_ne!(
                    moved.offset(),
                    before,
                    "OOM collection rewrites each fresh child"
                );
                let slot = if index == 0 { 0 } else { capacity.max(4) };
                assert_eq!(
                    object::layout_slot(current, &vm.gc_heap, slot),
                    Value::object(moved)
                );
                assert_eq!(
                    object::get_own(moved, &vm.gc_heap, "p0"),
                    Some(Value::number_i32(997))
                );
            }
            object::prevent_extensions(&mut current, &mut vm.gc_heap)
                .expect("ordinary retry after pressure removed");
            assert!(!object::is_extensible(current, &vm.gc_heap));
        });
    }
}

#[test]
fn collecting_prototype_change_roots_receiver_prototype_keys_and_both_banks() {
    for capacity in [0, 4, 64] {
        let mut vm =
            Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let (source, count) = source_shape(vm, capacity);
            let object =
                object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {})
                    .expect("source owner");
            let owner = vm.scoped_value(scope, Value::object(object));
            let alias = vm.scoped_value(scope, vm.escape_scoped(owner));
            let prototype = vm.scoped_object(scope).expect("fresh prototype");
            let first = vm.scoped_object(scope).expect("fresh prefix child");
            let last = vm.scoped_object(scope).expect("fresh suffix child");
            let marker = vm.scoped_value(scope, Value::number_i32(317));
            for child in [prototype, first, last] {
                vm.scoped_set(scope, child, "marker", marker)
                    .expect("fresh marker");
            }
            vm.scoped_set_slot(scope, owner, 0, first)
                .expect("prefix child");
            vm.scoped_set_slot(scope, owner, capacity.max(4), last)
                .expect("suffix child");
            let source_proto_shape = object::shape(
                vm.escape_scoped(prototype).as_object().unwrap(),
                &vm.gc_heap,
            );
            let source_proto_state = shape_body::state_of(source_proto_shape);
            assert!(
                source_proto_state.is_dictionary(),
                "marker insertion owns dictionary storage"
            );
            assert!(!source_proto_state.is_prototype());
            let offsets = [owner, prototype, first, last]
                .map(|value| vm.escape_scoped(value).as_object().unwrap().offset());
            let keys: Vec<_> = shape_body::shape_keys_ordered(&vm.gc_heap, source)
                .into_iter()
                .map(|(key, _)| key.offset())
                .collect();
            let locations: Vec<_> = (0..count)
                .map(|index| {
                    object::field_location_at(
                        vm.escape_scoped(owner).as_object().unwrap(),
                        &vm.gc_heap,
                        index as u32,
                    )
                })
                .collect();
            let _ = vm
                .gc_heap
                .alloc_old(Reclaimable([0; 16384]))
                .expect("reclaimable root funding");
            let reserved = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes();
            vm.gc_heap
                .reserve_bytes_no_collect(reserved)
                .expect("fill cap after fresh inputs");
            let cycles = vm.gc_heap.gc_cycle_counts();
            let mut current = vm.escape_scoped(owner).as_object().unwrap();
            let result = vm.set_ordinary_prototype(&mut current, Some(vm.escape_scoped(prototype)));
            vm.gc_heap.release_bytes(reserved);
            assert!(result.expect("collecting prototype change"));
            assert!(
                vm.gc_heap.gc_cycle_counts().1 > cycles.1,
                "prototype root/role preparation must collect"
            );
            assert_eq!(Value::object(current), vm.escape_scoped(owner));
            assert_eq!(vm.escape_scoped(owner), vm.escape_scoped(alias));
            for (value, old) in [owner, prototype, first, last].into_iter().zip(offsets) {
                assert_ne!(
                    vm.escape_scoped(value).as_object().unwrap().offset(),
                    old,
                    "each fresh rooted input really moved"
                );
            }
            let proto = vm.escape_scoped(prototype).as_object().unwrap();
            assert_eq!(object::prototype(current, &vm.gc_heap), Some(proto));
            assert_eq!(
                shape_body::state_of(source_proto_shape),
                source_proto_state,
                "old instance shape cannot change state or gain role in place"
            );
            assert!(object::state(proto, &vm.gc_heap).is_prototype());
            assert!(!object::state(current, &vm.gc_heap).is_prototype());
            for (index, old) in locations.into_iter().enumerate() {
                assert_eq!(
                    object::field_location_at(current, &vm.gc_heap, index as u32),
                    old
                );
            }
            for ((key, _), old) in shape_body::shape_keys_ordered(&vm.gc_heap, source)
                .into_iter()
                .zip(keys)
            {
                assert_ne!(key.offset(), old, "source lineage keys really moved");
            }
            for (slot, child) in [(0, first), (capacity.max(4), last)] {
                assert_eq!(
                    object::layout_slot(current, &vm.gc_heap, slot),
                    vm.escape_scoped(child)
                );
                assert_eq!(
                    object::get_own(
                        vm.escape_scoped(child).as_object().unwrap(),
                        &vm.gc_heap,
                        "marker"
                    ),
                    Some(Value::number_i32(317))
                );
            }
            let proof = object::prototype_validity::chain_validity(proto, &vm.gc_heap)
                .expect("registered ordinary prototype proof");
            let mut proto = proto;
            object::prevent_extensions(&mut proto, &mut vm.gc_heap)
                .expect("prototype state mutation");
            assert_eq!(Value::object(proto), vm.escape_scoped(prototype));
            assert!(
                !proof.is_valid(),
                "semantic role-owner state mutation retires the dependent proof"
            );
            let rebuilt =
                object::prototype_validity::chain_validity(proto, &vm.gc_heap).expect("new proof");
            object::prevent_extensions(&mut proto, &mut vm.gc_heap).expect("repeated state no-op");
            assert!(rebuilt.is_valid());
        });
    }
}

#[test]
fn provisional_descendants_keep_state_and_capacity_while_runtime_ics_remain_trainable() {
    for capacity in [0, 4, 64] {
        let mut vm = Interpreter::new().expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let initial = ShapeState::ORDINARY.with_provisional(true);
            let root = vm
                .shape_runtime
                .new_root(
                    &mut vm.gc_heap,
                    shape_body::ShapePrototype::Null,
                    capacity,
                    ShapeHandle::null(),
                    initial,
                    &mut |_| {},
                )
                .expect("family-owned provisional root");
            let shape = vm.shape_child(root, "p0").expect("provisional append");
            assert_eq!(shape_body::state_of(root), initial);
            assert_eq!(shape_body::state_of(shape), initial);
            assert_eq!(
                shape_body::state_of(shape_body::dictionary_of(shape)),
                initial.with_dictionary(true)
            );
            let object = object::alloc_object_with_shape_roots(&mut vm.gc_heap, shape, &mut |_| {})
                .expect("sampling instance");
            let owner = vm.scoped_value(scope, Value::object(object));
            let marker = vm.scoped_value(scope, Value::number_i32(719));
            vm.scoped_set_slot(scope, owner, 0, marker)
                .expect("initial field");
            let mut current = vm.escape_scoped(owner).as_object().unwrap();
            assert!(
                object::supports_fast_property_ic(current, &vm.gc_heap),
                "sampling must not poison runtime IC feedback"
            );
            assert!(object::state(current, &vm.gc_heap).is_provisional());
            object::prevent_extensions(&mut current, &mut vm.gc_heap)
                .expect("provisional state variant");
            assert!(object::state(current, &vm.gc_heap).is_provisional());
            assert!(!object::state(current, &vm.gc_heap).is_extensible());
            assert!(object::supports_fast_property_ic(current, &vm.gc_heap));
            object::freeze(&mut current, &mut vm.gc_heap)
                .expect("provisional dictionary integrity");
            let frozen = object::shape(current, &vm.gc_heap);
            let id = object::shape_id(current, &vm.gc_heap);
            assert!(object::is_dictionary(current, &vm.gc_heap));
            assert!(object::state(current, &vm.gc_heap).is_provisional());
            assert!(object::is_frozen(current, &vm.gc_heap));
            object::freeze(&mut current, &mut vm.gc_heap).expect("repeated freeze");
            assert_eq!(object::shape(current, &vm.gc_heap), frozen);
            assert_eq!(object::shape_id(current, &vm.gc_heap), id);
            let final_root = vm
                .shape_runtime
                .new_root(
                    &mut vm.gc_heap,
                    shape_body::ShapePrototype::Null,
                    capacity,
                    ShapeHandle::null(),
                    ShapeState::ORDINARY,
                    &mut |_| {},
                )
                .expect("independent finalized family");
            current = vm.escape_scoped(owner).as_object().unwrap();
            assert!(!shape_body::state_of(final_root).is_provisional());
            assert_ne!(final_root, root);
            assert_eq!(
                shape_body::state_of(root),
                initial,
                "old family remains provisional forever"
            );
            vm.gc_heap
                .read_payload(current, |body| assert_eq!(body.inline_capacity(), capacity));
            assert_eq!(
                object::get_own(current, &vm.gc_heap, "p0"),
                Some(Value::number_i32(719))
            );
        });
    }
}

#[test]
fn null_layout_cache_variants_never_change_late_ordinary_allocation_state() {
    for capacity in [0, 4, 64] {
        let mut vm =
            Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let initial = shape_body::null_root(&vm.gc_heap);
            assert_eq!(shape_body::state_of(initial), ShapeState::ORDINARY);
            assert_eq!(
                shape_body::inline_capacity_of(initial),
                object::DEFAULT_INLINE_CAPACITY
            );
            let (source, count) = source_shape(vm, capacity);
            let prototype =
                object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {})
                    .expect("fresh prototype");
            let prototype = vm.scoped_value(scope, Value::object(prototype));
            let alias = vm.scoped_value(scope, vm.escape_scoped(prototype));
            let children: Vec<_> = (0..2)
                .map(|index| {
                    let child =
                        object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {})
                            .expect("fresh child");
                    let child = vm.scoped_value(scope, Value::object(child));
                    let marker = vm.scoped_value(scope, Value::number_i32(941 + index));
                    vm.scoped_set_slot(scope, child, 0, marker)
                        .expect("child marker");
                    child
                })
                .collect();
            vm.scoped_set_slot(scope, prototype, 0, children[0])
                .unwrap();
            vm.scoped_set_slot(scope, prototype, count - 1, children[1])
                .unwrap();
            let before_prototype = vm.escape_scoped(prototype).as_object().unwrap().offset();
            let before_children: Vec<_> = children
                .iter()
                .map(|child| vm.escape_scoped(*child).as_object().unwrap().offset())
                .collect();
            assert_ne!(before_children[0], before_children[1]);
            let setup_cycles = vm.gc_heap.gc_cycle_counts();
            let _ = vm
                .gc_heap
                .alloc_old(Reclaimable([0; 16384]))
                .expect("reclaimable funding");
            assert_eq!(
                setup_cycles,
                vm.gc_heap.gc_cycle_counts(),
                "setup cannot promote fresh inputs"
            );
            let reserved = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes();
            vm.gc_heap
                .reserve_bytes_no_collect(reserved)
                .expect("fill cap");
            let current = vm.escape_scoped(prototype).as_object().unwrap();
            let instance_root = vm.object_root(Some(current), capacity, ShapeState::ORDINARY);
            vm.gc_heap.release_bytes(reserved);
            let instance_root = instance_root.expect("collecting prototype registration");
            assert!(vm.gc_heap.gc_cycle_counts().1 > setup_cycles.1);
            let current = vm.escape_scoped(prototype).as_object().unwrap();
            assert_ne!(before_prototype, current.offset());
            assert_eq!(vm.escape_scoped(prototype), vm.escape_scoped(alias));
            assert_eq!(
                vm.gc_heap
                    .read_payload(current, |body| body.inline_capacity()),
                capacity
            );
            assert!(object::state(current, &vm.gc_heap).is_prototype());
            assert!(
                vm.gc_heap
                    .read_payload(current, |body| body.exotic().is_some())
            );
            assert_eq!(
                object::cached_instance_root(current, &vm.gc_heap),
                Some(instance_root)
            );
            for (index, (child, before)) in children.into_iter().zip(before_children).enumerate() {
                let child = vm.escape_scoped(child).as_object().unwrap();
                assert_ne!(before, child.offset());
                assert_eq!(
                    object::layout_slot(
                        current,
                        &vm.gc_heap,
                        if index == 0 { 0 } else { count - 1 }
                    ),
                    Value::object(child)
                );
                assert_eq!(
                    object::get_own(child, &vm.gc_heap, "p0"),
                    Some(Value::number_i32(941 + index as i32))
                );
            }
            let prototype_state = ShapeState::ORDINARY.with_prototype_role(true);
            let role_root = shape_body::root_for_layout(
                shape_body::null_root_head(&vm.gc_heap),
                capacity,
                prototype_state,
            )
            .expect("prototype role root is cached");
            let closed = object::heap_instance_root(
                object::ObjectPrototype::Null,
                &mut vm.gc_heap,
                capacity,
                ShapeState::ORDINARY.with_extensible(false),
                &mut |_| {},
            )
            .expect("closed root variant");
            let provisional = object::heap_instance_root(
                object::ObjectPrototype::Null,
                &mut vm.gc_heap,
                capacity,
                ShapeState::ORDINARY.with_provisional(true),
                &mut |_| {},
            )
            .expect("independent provisional root");
            assert!(shape_body::state_of(provisional).is_provisional());
            assert_eq!(shape_body::null_root_head(&vm.gc_heap), closed);
            assert_ne!(closed, initial);
            assert_eq!(shape_body::null_root(&vm.gc_heap), initial);
            assert_eq!(
                shape_body::root_for_layout(closed, capacity, prototype_state),
                Some(role_root)
            );
            // Heap-only late dictionary allocation used to inherit the newest
            // cached prototype role, although it had no prototype sidecar.
            let late = object::alloc_dictionary_object_with_roots(&mut vm.gc_heap, &mut |_| {})
                .expect("late ordinary dictionary");
            let late = vm.scoped_value(scope, Value::object(late));
            let mut late_current = vm.escape_scoped(late).as_object().unwrap();
            assert_eq!(
                object::state(late_current, &vm.gc_heap),
                ShapeState::ORDINARY.with_dictionary(true)
            );
            assert_eq!(
                vm.gc_heap
                    .read_payload(late_current, |body| body.inline_capacity()),
                object::DEFAULT_INLINE_CAPACITY
            );
            assert!(
                object::define_own_property_in_place(
                    &mut late_current,
                    &mut vm.gc_heap,
                    "field",
                    object::PropertyDescriptor::data(Value::number_i32(73), true, true, true),
                )
                .expect("late descriptor mutation")
            );
            assert_eq!(
                object::get_own(late_current, &vm.gc_heap, "field"),
                Some(Value::number_i32(73))
            );
            vm.force_gc()
                .expect("collect cached role and capacity variants");
            assert_eq!(shape_body::null_root(&vm.gc_heap), initial);
            assert_eq!(
                shape_body::root_for_layout(
                    shape_body::null_root_head(&vm.gc_heap),
                    capacity,
                    prototype_state
                ),
                Some(role_root)
            );
            assert_eq!(
                shape_body::root_for_layout(
                    shape_body::null_root_head(&vm.gc_heap),
                    capacity,
                    ShapeState::ORDINARY.with_extensible(false)
                ),
                Some(closed)
            );
        });
    }
}
