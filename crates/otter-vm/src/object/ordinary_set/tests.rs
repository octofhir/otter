//! Actual collection, cap failure and noallocation ordinary Set proofs.
//!
//! # Contents
//! - Persistent capacity 0/4/64 banks survive descriptor preparation collection.
//! - Allocation failure preserves descriptors and reports its actual cap cause.
//! - Assignment rejection preserves attributes; construction uses its own definition semantics.
//!
//! # Invariants
//! - Every allocating fixture uses the production interpreter handle scope.
//! - Fresh input children are installed/compared through current canonical homes.
//! - Real collection is caused by the descriptor owner, after pressure admission.
//! - No assertion substitutes an aggregate collection count for child motion.
//!
//! # See also
//! - `super::super::descriptor_install` owns the sole publication algorithm.

use crate::object::{
    self, DescriptorKind, FieldLayout, FieldLocation, PropertyDescriptor, ShapeHandle, ShapeState,
    shape_body,
};
use crate::{Interpreter, Value};
use otter_gc::SafeTraceable;

struct Reclaimable([u64; 16384]);
impl SafeTraceable for Reclaimable {
    const TYPE_TAG: u8 = 0xea;
    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        let _ = self.0[0];
    }
}

fn bank_shape(vm: &mut Interpreter, capacity: usize) -> (ShapeHandle, usize) {
    let count = capacity + 8;
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
        .expect("bank root");
    for index in 0..count {
        shape = vm
            .shape_child(shape, &format!("p{index}"))
            .expect("bank child");
    }
    (shape, count)
}

#[test]
fn collecting_ordinary_append_rewrites_banks_and_pending_shape_without_turn_pins() {
    for (capacity, prepared) in [0, 4, 64]
        .into_iter()
        .flat_map(|capacity| [(capacity, false), (capacity, true)])
    {
        let mut vm =
            Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let (source, count) = bank_shape(vm, capacity);
            let owner = object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {})
                .expect("bank owner");
            let owner = vm.scoped_value(scope, Value::object(owner));
            let alias = vm.scoped_value(scope, vm.escape_scoped(owner));
            let children: Vec<_> = (0..3)
                .map(|_| {
                    let child =
                        object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {})
                            .expect("fresh shaped child");
                    vm.scoped_value(scope, Value::object(child))
                })
                .collect();
            for (index, child) in children.iter().copied().enumerate() {
                let marker = vm.scoped_value(scope, Value::number_i32(317 + index as i32));
                vm.scoped_set_slot(scope, child, 0, marker)
                    .expect("nonallocating child marker");
            }
            vm.scoped_set_slot(scope, owner, 0, children[0]).unwrap();
            vm.scoped_set_slot(scope, owner, count - 1, children[1])
                .unwrap();
            let before: Vec<_> = children
                .iter()
                .map(|child| vm.escape_scoped(*child).as_object().unwrap().offset())
                .collect();
            assert_ne!(before[0], before[1]);
            assert_ne!(before[1], before[2]);
            assert_ne!(before[0], before[2]);
            let locations: Vec<_> = (0..count)
                .map(|index| {
                    object::field_location_at(
                        vm.escape_scoped(owner).as_object().unwrap(),
                        &vm.gc_heap,
                        index as u32,
                    )
                })
                .collect();
            let next_shape =
                prepared.then(|| vm.shape_child(source, "incoming").expect("prepared append"));
            let setup_cycles = vm.gc_heap.gc_cycle_counts();
            let _ = vm
                .gc_heap
                .alloc_old(Reclaimable([0; 16384]))
                .expect("reclaimable pressure");
            assert_eq!(
                setup_cycles,
                vm.gc_heap.gc_cycle_counts(),
                "setup never promotes the fresh inputs"
            );
            // Weak transition caches and released turn pins cannot keep the
            // provided shape alive; the production pending slot must do it.
            vm.shape_runtime.unpin_turn_shapes();
            let reserved = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes();
            vm.gc_heap
                .reserve_bytes_no_collect(reserved)
                .expect("fill cap without collecting");
            let cycles = vm.gc_heap.gc_cycle_counts();
            let mut current = vm.escape_scoped(owner).as_object().unwrap();
            let incoming = vm.escape_scoped(children[2]);
            let accepted = if let Some(shape) = next_shape {
                object::ordinary_set_data_property_with_shape(
                    &mut current,
                    &mut vm.gc_heap,
                    "incoming",
                    incoming,
                    shape,
                )
            } else {
                object::ordinary_set_data_property(
                    &mut current,
                    &mut vm.gc_heap,
                    "incoming",
                    incoming,
                )
            }
            .expect("ordinary assignment-triggered collecting append");
            vm.gc_heap.release_bytes(reserved);
            assert!(accepted);
            assert!(vm.gc_heap.gc_cycle_counts().1 > cycles.1);
            assert_eq!(Value::object(current), vm.escape_scoped(owner));
            assert_eq!(vm.escape_scoped(owner), vm.escape_scoped(alias));
            assert_eq!(object::is_dictionary(current, &vm.gc_heap), !prepared);
            if let Some(next_shape) = next_shape {
                assert_eq!(object::shape(current, &vm.gc_heap), next_shape);
                assert_eq!(
                    shape_body::shape_property_count(&vm.gc_heap, next_shape),
                    count as u32 + 1
                );
            }
            assert_eq!(shape_body::state_of(source), ShapeState::ORDINARY);
            vm.gc_heap
                .read_payload(current, |body| assert_eq!(body.inline_capacity(), capacity));
            for (index, location) in locations.into_iter().enumerate() {
                assert_eq!(
                    object::field_location_at(current, &vm.gc_heap, index as u32),
                    location
                );
            }
            for (index, child) in children.into_iter().enumerate() {
                let child = vm.escape_scoped(child).as_object().unwrap();
                assert_ne!(
                    child.offset(),
                    before[index],
                    "exact fresh child actually moved"
                );
                let name = match index {
                    0 => "p0".to_owned(),
                    1 => format!("p{}", count - 1),
                    _ => "incoming".to_owned(),
                };
                assert_eq!(
                    object::get_own(current, &vm.gc_heap, &name),
                    Some(Value::object(child))
                );
                assert_eq!(
                    object::get_own(child, &vm.gc_heap, "p0"),
                    Some(Value::number_i32(317 + index as i32))
                );
            }
        });
    }
}

#[test]
fn ordinary_set_oom_retains_cause_and_rewrites_all_live_aliases() {
    for capacity in [0, 4, 64] {
        let mut vm =
            Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let (source, count) = bank_shape(vm, capacity);
            let owner = object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {}).unwrap();
            let owner = vm.scoped_value(scope, Value::object(owner));
            let alias = vm.scoped_value(scope, vm.escape_scoped(owner));
            // The owner traces the source lineage; settle all prior temporary
            // allocations before admitting only live fresh bank inputs.
            vm.force_gc().expect("settle bootstrap and source owner");
            let geometry = FieldLayout::current();
            let input_bytes = 2 * (geometry.cell_bytes(capacity) as u64
                + u64::from(geometry.slab_words_byte)
                + FieldLocation::words_bytes(count - capacity) as u64);
            let reserved = vm.gc_heap.max_heap_bytes()
                - vm.gc_heap.stats().allocated_bytes as u64
                - input_bytes - 1;
            vm.gc_heap.reserve_bytes_with_roots(reserved, &mut |_| {})
                .expect("admit exact live inputs before fresh bank children");
            let admitted = vm.gc_heap.stats();
            let cycles = vm.gc_heap.gc_cycle_counts();
            let mut children = Vec::new();
            for index in 0..2 {
                let child = object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {}).unwrap();
                let child = vm.scoped_value(scope, Value::object(child));
                let marker = vm.scoped_value(scope, Value::number_i32(719 + index));
                vm.scoped_set_slot(scope, child, 0, marker).unwrap();
                children.push(child);
            }
            let first = children[0]; let last = children[1];
            vm.scoped_set_slot(scope, owner, 0, first).unwrap();
            vm.scoped_set_slot(scope, owner, count - 1, last).unwrap();
            let before = [vm.escape_scoped(first).as_object().unwrap().offset(), vm.escape_scoped(last).as_object().unwrap().offset()];
            assert_ne!(before[0], before[1]);
            assert_eq!(vm.gc_heap.gc_cycle_counts(), cycles, "fresh bank inputs cannot age during setup");
            assert_eq!(vm.gc_heap.stats().allocated_bytes - admitted.allocated_bytes, input_bytes as usize);
            assert_eq!(vm.gc_heap.tracked_bytes(), vm.gc_heap.max_heap_bytes() - 1);
            let mut current = vm.escape_scoped(owner).as_object().unwrap();
            let incoming = vm.escape_scoped(last);
            let error = object::ordinary_set_data_property(
                &mut current, &mut vm.gc_heap, "deniedAllocation", incoming,
            ).expect_err("ordinary Set allocator failure must not become Ok(false)");
            vm.gc_heap.release_bytes(reserved);
            assert!(error.requested_bytes() > 0); assert_eq!(error.heap_limit_bytes(), vm.gc_heap.max_heap_bytes());
            assert!(vm.gc_heap.gc_cycle_counts().1 > cycles.1);
            assert_eq!(Value::object(current), vm.escape_scoped(owner));
            assert_eq!(vm.escape_scoped(owner), vm.escape_scoped(alias));
            assert_eq!(object::shape(current, &vm.gc_heap), source);
            assert!(object::get_own_descriptor(current, &vm.gc_heap, "deniedAllocation").is_none());
            for (index, child) in [first, last].into_iter().enumerate() {
                let child = vm.escape_scoped(child).as_object().unwrap();
                assert_ne!(child.offset(), before[index], "failure collection rewrites exact fresh child");
                let key = if index == 0 { "p0".to_owned() } else { format!("p{}", count - 1) };
                let descriptor = object::get_own_descriptor(current, &vm.gc_heap, &key).unwrap();
                assert!(descriptor.writable() && descriptor.enumerable() && descriptor.configurable());
                assert!(matches!(descriptor.kind, DescriptorKind::Data { value } if value == Value::object(child)));
                assert_eq!(object::get_own(child, &vm.gc_heap, "p0"), Some(Value::number_i32(719 + index as i32)));
            }
        });
    }
}

#[test]
fn mapped_own_value_equal_write_through_and_rejection_do_not_allocate() {
    let mut vm = Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("fixture interpreter");
    vm.gc_heap.set_gc_stress(0, false);
    vm.with_handle_scope(|vm, scope| {
        let owner = vm.scoped_object(scope).unwrap();
        let mut current = vm.escape_scoped(owner).as_object().unwrap();
        assert!(
            object::define_own_property_in_place(
                &mut current,
                &mut vm.gc_heap,
                "0",
                PropertyDescriptor::data(Value::number_i32(11), true, false, false)
            )
            .unwrap()
        );
        assert!(
            object::define_own_property_in_place(
                &mut current,
                &mut vm.gc_heap,
                "readonly",
                PropertyDescriptor::data(Value::number_i32(7), false, true, false)
            )
            .unwrap()
        );
        assert!(
            object::define_own_property_in_place(
                &mut current,
                &mut vm.gc_heap,
                "accessor",
                PropertyDescriptor::accessor(None, None, true, true)
            )
            .unwrap()
        );
        let context = crate::context::alloc_context_with_roots(
            &mut vm.gc_heap,
            crate::context::ContextShape {
                scope_function_id: 0,
                scope_index: 0,
                slot_count: 1,
                has_extension: false,
            },
            Value::undefined(),
            |_| false,
            &mut |_| {},
        )
        .unwrap();
        let context = vm.scoped_value(scope, Value::context(context));
        let context_handle = vm.escape_scoped(context).as_context().unwrap();
        assert!(crate::context::write_slot(
            &mut vm.gc_heap,
            context_handle,
            0,
            Value::number_i32(3)
        ));
        current = vm.escape_scoped(owner).as_object().unwrap();
        object::install_mapped_arguments(
            &mut current,
            &mut vm.gc_heap,
            object::MappedArguments {
                context: context_handle,
                entries: vec![object::MappedArgumentEntry {
                    key: "0".to_owned(),
                    slot: 0,
                }],
            },
        )
        .unwrap();
        object::prevent_extensions(&mut current, &mut vm.gc_heap).unwrap();
        assert!(object::watch_dictionary_slot(current, &mut vm.gc_heap, 0));
        let watched = object::dictionary_layout(current, &vm.gc_heap).unwrap();
        let shape_id = object::shape_id(current, &vm.gc_heap);
        let reserved = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes();
        vm.gc_heap.reserve_bytes_no_collect(reserved).unwrap();
        let cycles = vm.gc_heap.gc_cycle_counts();
        // Stored own value is already11 while the parameter is3: identical
        // descriptor detection must still commit this aliased assignment.
        assert!(
            object::ordinary_set_data_property(
                &mut current,
                &mut vm.gc_heap,
                "0",
                Value::number_i32(11)
            )
            .unwrap()
        );
        assert!(
            !object::ordinary_set_data_property(
                &mut current,
                &mut vm.gc_heap,
                "readonly",
                Value::number_i32(8)
            )
            .unwrap()
        );
        assert!(
            !object::ordinary_set_data_property(
                &mut current,
                &mut vm.gc_heap,
                "accessor",
                Value::number_i32(8)
            )
            .unwrap()
        );
        assert!(
            !object::ordinary_set_data_property(
                &mut current,
                &mut vm.gc_heap,
                "absent",
                Value::number_i32(8)
            )
            .unwrap()
        );
        vm.gc_heap.release_bytes(reserved);
        assert_eq!(cycles, vm.gc_heap.gc_cycle_counts());
        assert_eq!(shape_id, object::shape_id(current, &vm.gc_heap));
        let context_handle = vm.escape_scoped(context).as_context().unwrap();
        assert_eq!(
            crate::context::read_slot(&vm.gc_heap, context_handle, 0),
            Some(Value::number_i32(11))
        );
        assert_eq!(
            object::get_own(current, &vm.gc_heap, "0"),
            Some(Value::number_i32(11))
        );
        let descriptor = object::get_own_descriptor(current, &vm.gc_heap, "0").unwrap();
        assert_eq!(
            descriptor.flags,
            object::PropertyFlags::new(true, false, false)
        );
        vm.gc_heap.read_payload(current, |body| {
            assert!(body.slots()[0].watched);
            assert_eq!(body.exotic().unwrap().dictionary_layout, watched);
        });
        assert!(matches!(
            object::get_own_descriptor(current, &vm.gc_heap, "accessor")
                .unwrap()
                .kind,
            DescriptorKind::Accessor {
                getter: None,
                setter: None
            }
        ));
    });
}

#[test]
fn collecting_symbol_append_traces_current_key_description_and_distinct_payloads() {
    for capacity in [0, 4, 64] {
        let mut vm =
            Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let (source, count) = bank_shape(vm, capacity);
            let owner = object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {}).unwrap();
            let owner = vm.scoped_value(scope, Value::object(owner));
            let alias = vm.scoped_value(scope, vm.escape_scoped(owner));
            let description = vm.scoped_string(scope, "ordinary Set key").unwrap();
            let string = vm.escape_scoped(description).as_string(&vm.gc_heap).unwrap();
            let symbol = crate::symbol::JsSymbol::new(&mut vm.gc_heap, Some(string)).unwrap();
            let symbol = vm.scoped_value(scope, Value::symbol(symbol));
            let mut children = Vec::new();
            for index in 0..3 {
                let child = object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {}).unwrap();
                let child = vm.scoped_value(scope, Value::object(child));
                let marker = vm.scoped_value(scope, Value::number_i32(913 + index));
                vm.scoped_set_slot(scope, child, 0, marker).unwrap();
                children.push(child);
            }
            vm.scoped_set_slot(scope, owner, 0, children[0]).unwrap();
            vm.scoped_set_slot(scope, owner, count - 1, children[1]).unwrap();
            let before: Vec<_> = children.iter().map(|child| vm.escape_scoped(*child).as_object().unwrap().offset()).collect();
            assert_ne!(before[0], before[1]); assert_ne!(before[1], before[2]); assert_ne!(before[0], before[2]);
            let description_before = vm.escape_scoped(symbol).as_symbol(&vm.gc_heap).unwrap().description().unwrap().handle().offset();
            let _ = vm.gc_heap.alloc_old(Reclaimable([0; 16384])).unwrap();
            let reserved = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes();
            vm.gc_heap.reserve_bytes_no_collect(reserved).unwrap();
            let cycles = vm.gc_heap.gc_cycle_counts();
            let mut current = vm.escape_scoped(owner).as_object().unwrap();
            let key = vm.escape_scoped(symbol).as_symbol(&vm.gc_heap).unwrap();
            let incoming = vm.escape_scoped(children[2]);
            assert!(object::ordinary_set_symbol_data_property(&mut current, &mut vm.gc_heap, key, incoming).unwrap());
            vm.gc_heap.release_bytes(reserved);
            assert!(vm.gc_heap.gc_cycle_counts().1 > cycles.1);
            assert_eq!(Value::object(current), vm.escape_scoped(alias));
            let key = vm.escape_scoped(symbol).as_symbol(&vm.gc_heap).unwrap();
            let description = key.description().unwrap();
            assert_ne!(description.handle().offset(), description_before, "exact fresh key description relocated");
            assert_eq!(description.to_lossy_string(&vm.gc_heap), "ordinary Set key");
            assert_eq!(object::shape(current, &vm.gc_heap), source, "symbol key does not alter named geometry");
            for (index, child) in children.into_iter().enumerate() {
                let child = vm.escape_scoped(child).as_object().unwrap();
                assert_ne!(child.offset(), before[index], "exact bank/pending child relocated");
                let value = if index == 2 {
                    match object::lookup_own_symbol(current, &vm.gc_heap, key) {
                        object::PropertyLookup::Data { value, flags } => {
                            assert_eq!(flags, object::PropertyFlags::data_default()); value
                        }, _ => panic!("new symbol is an ordinary data descriptor"),
                    }
                } else {
                    object::get_own(current, &vm.gc_heap, &if index == 0 { "p0".to_owned() } else { format!("p{}", count - 1) }).unwrap()
                };
                assert_eq!(value, Value::object(child));
                assert_eq!(object::get_own(child, &vm.gc_heap, "p0"), Some(Value::number_i32(913 + index as i32)));
            }
            let reserved = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes();
            vm.gc_heap.reserve_bytes_no_collect(reserved).unwrap();
            let cycles = vm.gc_heap.gc_cycle_counts();
            assert!(object::ordinary_set_symbol_data_property(&mut current, &mut vm.gc_heap, key, Value::number_i32(777)).unwrap());
            vm.gc_heap.release_bytes(reserved);
            assert_eq!(cycles, vm.gc_heap.gc_cycle_counts(), "existing symbol value overwrite does not allocate");
            assert!(matches!(object::lookup_own_symbol(current, &vm.gc_heap, key), object::PropertyLookup::Data { value, .. } if value == Value::number_i32(777)));
        });
    }
}

#[test]
fn child_preparation_reports_cap_oom_without_bypass_and_rewrites_pending_inputs() {
    for capacity in [0, 4, 64] {
        let mut vm =
            Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let (source, count) = bank_shape(vm, capacity);
            let owner = object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {})
                .unwrap();
            let owner = vm.scoped_value(scope, Value::object(owner));
            let alias = vm.scoped_value(scope, vm.escape_scoped(owner));
            vm.force_gc().expect("settle bootstrap and admitted owner");
            let geometry = FieldLayout::current();
            let input_bytes = 3
                * (geometry.cell_bytes(capacity) as u64
                    + u64::from(geometry.slab_words_byte)
                    + FieldLocation::words_bytes(count - capacity) as u64);
            let reserved = vm.gc_heap.max_heap_bytes()
                - vm.gc_heap.stats().allocated_bytes as u64
                - input_bytes
                - 1;
            vm.gc_heap
                .reserve_bytes_with_roots(reserved, &mut |_| {})
                .expect("admit exact pending shape inputs before nursery allocation");
            let admitted = vm.gc_heap.stats();
            let cycles = vm.gc_heap.gc_cycle_counts();
            let mut children = Vec::new();
            for index in 0..3 {
                let child =
                    object::alloc_object_with_shape_roots(&mut vm.gc_heap, source, &mut |_| {})
                        .unwrap();
                let child = vm.scoped_value(scope, Value::object(child));
                let marker = vm.scoped_value(scope, Value::number_i32(1201 + index));
                vm.scoped_set_slot(scope, child, 0, marker).unwrap();
                children.push(child);
            }
            vm.scoped_set_slot(scope, owner, 0, children[0]).unwrap();
            vm.scoped_set_slot(scope, owner, count - 1, children[1])
                .unwrap();
            let before: Vec<_> = children
                .iter()
                .map(|child| vm.escape_scoped(*child).as_object().unwrap().offset())
                .collect();
            assert_ne!(before[0], before[1]);
            assert_ne!(before[1], before[2]);
            assert_ne!(before[0], before[2]);
            assert_eq!(
                vm.gc_heap.gc_cycle_counts(),
                cycles,
                "pending inputs cannot age before child preparation"
            );
            assert_eq!(
                vm.gc_heap.stats().allocated_bytes - admitted.allocated_bytes,
                input_bytes as usize
            );
            assert_eq!(vm.gc_heap.tracked_bytes(), vm.gc_heap.max_heap_bytes() - 1);
            let mut current = vm.escape_scoped(owner).as_object().unwrap();
            let mut pending = vm.escape_scoped(children[2]);
            let error = vm
                .shape_child_rooting_object_value(
                    source,
                    "neverAdmittedChild",
                    &mut current,
                    &mut pending,
                )
                .expect_err("shape preparation must respect the canonical heap cap");
            vm.gc_heap.release_bytes(reserved);
            match error {
                crate::VmError::OutOfMemory {
                    requested_bytes,
                    heap_limit_bytes,
                } => {
                    assert!(requested_bytes > 0);
                    assert_eq!(heap_limit_bytes, vm.gc_heap.max_heap_bytes());
                }
                other => panic!("actual shape allocation cause retained: {other:?}"),
            }
            assert!(vm.gc_heap.gc_cycle_counts().1 > cycles.1);
            assert_eq!(Value::object(current), vm.escape_scoped(owner));
            assert_eq!(vm.escape_scoped(owner), vm.escape_scoped(alias));
            assert_eq!(
                pending,
                vm.escape_scoped(children[2]),
                "caller pending slot rewritten on failure"
            );
            assert_eq!(object::shape(current, &vm.gc_heap), source);
            assert!(
                object::get_own_descriptor(current, &vm.gc_heap, "neverAdmittedChild").is_none()
            );
            for (index, child) in children.into_iter().enumerate() {
                let child = vm.escape_scoped(child).as_object().unwrap();
                assert_ne!(
                    child.offset(),
                    before[index],
                    "exact fresh child relocated by failed preparation collection"
                );
                assert_eq!(
                    object::get_own(child, &vm.gc_heap, "p0"),
                    Some(Value::number_i32(1201 + index as i32))
                );
                if index != 2 {
                    let name = if index == 0 {
                        "p0".to_owned()
                    } else {
                        format!("p{}", count - 1)
                    };
                    assert_eq!(
                        object::get_own(current, &vm.gc_heap, &name),
                        Some(Value::object(child))
                    );
                }
            }
        });
    }
}

#[test]
fn creation_replaces_configurable_accessor_while_assignment_preserves_descriptor() {
    let mut vm = Interpreter::new().expect("fixture interpreter");
    vm.with_handle_scope(|vm, scope| {
        let owner = vm.scoped_object(scope).unwrap();
        let mut current = vm.escape_scoped(owner).as_object().unwrap();
        assert!(object::define_own_property_in_place(&mut current, &mut vm.gc_heap, "member", PropertyDescriptor::accessor(None, None, false, true)).unwrap());
        assert!(object::define_own_property_in_place(&mut current, &mut vm.gc_heap, "locked", PropertyDescriptor::data(Value::number_i32(19), false, false, false)).unwrap());
        let old_shape = object::shape_id(current, &vm.gc_heap);
        assert!(!object::ordinary_set_data_property(&mut current, &mut vm.gc_heap, "member", Value::number_i32(23)).unwrap());
        assert_eq!(object::shape_id(current, &vm.gc_heap), old_shape);
        assert!(matches!(object::get_own_descriptor(current, &vm.gc_heap, "member").unwrap().kind, DescriptorKind::Accessor { getter: None, setter: None }));
        vm.create_data_property(&mut current, "member", Value::number_i32(23)).expect("CreateDataPropertyOrThrow replaces configurable accessor");
        assert_eq!(Value::object(current), vm.escape_scoped(owner));
        let replaced = object::get_own_descriptor(current, &vm.gc_heap, "member").unwrap();
        assert_eq!(replaced.flags, object::PropertyFlags::data_default());
        assert!(matches!(replaced.kind, DescriptorKind::Data { value } if value == Value::number_i32(23)));
        let error = vm.create_data_property(&mut current, "locked", Value::number_i32(29)).expect_err("creation rejects nonconfigurable readonly descriptor");
        assert!(matches!(error, crate::VmError::TypeMismatch));
        let unchanged = object::get_own_descriptor(current, &vm.gc_heap, "locked").unwrap();
        assert_eq!(unchanged.flags, object::PropertyFlags::new(false, false, false));
        assert!(matches!(unchanged.kind, DescriptorKind::Data { value } if value == Value::number_i32(19)));
    });
}
