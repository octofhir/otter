//! Typed descriptor rejection, allocation failure and actual relocation proofs.
//!
//! # Contents
//! - Persistent capacity 0/4/64 banks survive descriptor preparation collection.
//! - Allocation failure preserves descriptors and reports its actual cap cause.
//! - Identical/rejected descriptors preserve watches and do not allocate.
//!
//! # Invariants
//! - Every allocating fixture uses the production interpreter handle scope.
//! - Fresh input children are installed/compared through current canonical homes.
//! - Real collection is caused by the descriptor owner, after pressure admission.
//! - No assertion substitutes an aggregate collection count for child motion.
//!
//! # See also
//! - `super` owns the one string/symbol descriptor publication algorithm.

use crate::object::{
    self, DescriptorKind, FieldLayout, FieldLocation, PartialPropertyDescriptor,
    PropertyDescriptor, ShapeHandle, ShapeState, shape_body,
};
use crate::{Interpreter, Value};
use otter_gc::SafeTraceable;

struct Reclaimable([u64; 16384]);
impl SafeTraceable for Reclaimable {
    const TYPE_TAG: u8 = 0xe8;
    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        let _ = self.0[0];
    }
}

fn bank_shape(vm: &mut Interpreter, capacity: usize) -> (ShapeHandle, usize) {
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
        .expect("bank root");
    for index in 0..count {
        shape = vm
            .shape_child(shape, &format!("p{index}"))
            .expect("bank child");
    }
    (shape, count)
}

#[test]
fn collecting_string_append_preserves_banks_and_rewrites_exact_incoming_payload() {
    for capacity in [0, 4, 64] {
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
            let _ = vm
                .gc_heap
                .alloc_old(Reclaimable([0; 16384]))
                .expect("reclaimable pressure");
            let reserved = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes();
            vm.gc_heap
                .reserve_bytes_no_collect(reserved)
                .expect("fill cap without collecting");
            let cycles = vm.gc_heap.gc_cycle_counts();
            let mut current = vm.escape_scoped(owner).as_object().unwrap();
            let descriptor = PartialPropertyDescriptor::from_full(&PropertyDescriptor::data(
                vm.escape_scoped(children[2]),
                true,
                true,
                true,
            ));
            let accepted = object::define_own_property_partial(
                &mut current,
                &mut vm.gc_heap,
                "incoming",
                descriptor,
            )
            .expect("descriptor-triggered collecting append");
            vm.gc_heap.release_bytes(reserved);
            assert!(accepted);
            assert!(vm.gc_heap.gc_cycle_counts().1 > cycles.1);
            assert_eq!(Value::object(current), vm.escape_scoped(owner));
            assert_eq!(vm.escape_scoped(owner), vm.escape_scoped(alias));
            assert!(object::is_dictionary(current, &vm.gc_heap));
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
fn descriptor_oom_preserves_source_and_updates_all_live_bank_aliases() {
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
            let one_input = geometry.cell_bytes(capacity) as u64
                + u64::from(geometry.slab_words_byte)
                + FieldLocation::words_bytes(count - capacity) as u64;
            let input_bytes = 2 * one_input;
            // Explicit collection can leave conservatively charged swept setup
            // bytes. Admit against actual retained cells before fresh children
            // exist, using the collecting accounting owner to reconcile them.
            let reserved = vm.gc_heap.max_heap_bytes()
                - vm.gc_heap.stats().allocated_bytes as u64
                - input_bytes
                - 1;
            vm.gc_heap.reserve_bytes_with_roots(reserved, &mut |_| {})
                .expect("admit exact live input budget before fresh children");
            let admitted = vm.gc_heap.stats();
            assert_eq!(admitted.tracked_bytes, admitted.allocated_bytes as u64 + admitted.reserved_bytes);
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
            vm.gc_heap.read_payload(current, |body| {
                assert!(body.exotic.is_null(), "fresh shaped owner still requires descriptor sidecar allocation");
            });
            let descriptor = PartialPropertyDescriptor::from_full(&PropertyDescriptor::data(vm.escape_scoped(last), true, true, true));
            let error = object::define_own_property_partial(
                &mut current, &mut vm.gc_heap, "deniedAllocation",
                descriptor,
            ).expect_err("allocator failure must not become Ok(false)");
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
fn no_op_rejection_and_symbol_kinds_preserve_watched_string_epoch_at_full_cap() {
    let mut vm = Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("fixture interpreter");
    vm.gc_heap.set_gc_stress(0, false);
    vm.with_handle_scope(|vm, scope| {
        let owner = vm.scoped_object(scope).unwrap();
        let mut current = vm.escape_scoped(owner).as_object().unwrap();
        let stable = PropertyDescriptor::data(Value::number_i32(7), false, true, false);
        assert!(
            object::define_own_property_in_place(
                &mut current,
                &mut vm.gc_heap,
                "stable",
                stable.clone()
            )
            .unwrap()
        );
        let symbol = crate::symbol::JsSymbol::new(&mut vm.gc_heap, None).unwrap();
        let symbol = vm.scoped_value(scope, Value::symbol(symbol));
        let private = crate::symbol::JsSymbol::new_private(&mut vm.gc_heap, None).unwrap();
        let private = vm.scoped_value(scope, Value::symbol(private));
        current = vm.escape_scoped(owner).as_object().unwrap();
        let symbol_key = vm.escape_scoped(symbol).as_symbol(&vm.gc_heap).unwrap();
        assert!(
            object::define_own_symbol_property_partial(
                &mut current,
                &mut vm.gc_heap,
                symbol_key,
                PartialPropertyDescriptor::from_full(&PropertyDescriptor::accessor(
                    None, None, true, true
                ))
            )
            .unwrap()
        );
        let private_key = vm.escape_scoped(private).as_symbol(&vm.gc_heap).unwrap();
        assert!(
            object::define_own_symbol_property_partial(
                &mut current,
                &mut vm.gc_heap,
                private_key,
                PartialPropertyDescriptor::from_full(&PropertyDescriptor::data(
                    Value::number_i32(9),
                    true,
                    true,
                    true
                ))
            )
            .unwrap()
        );
        object::freeze(&mut current, &mut vm.gc_heap).unwrap();
        assert!(object::watch_dictionary_slot(current, &mut vm.gc_heap, 0));
        let before = vm.gc_heap.read_payload(current, |body| {
            (body.dictionary_shape_id(), body.dictionary_layout())
        });
        vm.force_gc().unwrap();
        current = vm.escape_scoped(owner).as_object().unwrap();
        let reserved = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes();
        vm.gc_heap.reserve_bytes_no_collect(reserved).unwrap();
        let cycles = vm.gc_heap.gc_cycle_counts();
        assert!(
            object::define_own_property_partial(
                &mut current,
                &mut vm.gc_heap,
                "stable",
                PartialPropertyDescriptor::default()
            )
            .unwrap()
        );
        assert!(
            object::define_own_property_in_place(&mut current, &mut vm.gc_heap, "stable", stable)
                .unwrap()
        );
        assert!(
            !object::define_own_property_partial(
                &mut current,
                &mut vm.gc_heap,
                "stable",
                PartialPropertyDescriptor {
                    writable: Some(true),
                    ..Default::default()
                }
            )
            .unwrap()
        );
        assert!(
            !object::define_own_property_partial(
                &mut current,
                &mut vm.gc_heap,
                "new",
                PartialPropertyDescriptor {
                    value: Some(Value::number_i32(11)),
                    ..Default::default()
                }
            )
            .unwrap()
        );
        let key = vm.escape_scoped(symbol).as_symbol(&vm.gc_heap).unwrap();
        assert!(
            object::define_own_symbol_property_partial(
                &mut current,
                &mut vm.gc_heap,
                key,
                PartialPropertyDescriptor::from_full(&PropertyDescriptor::accessor(
                    None, None, true, false
                ))
            )
            .unwrap()
        );
        assert_eq!(
            vm.gc_heap.gc_cycle_counts(),
            cycles,
            "no-op/reject paths allocate nothing"
        );
        assert_eq!(
            vm.gc_heap.read_payload(current, |body| (
                body.dictionary_shape_id(),
                body.dictionary_layout()
            )),
            before
        );
        vm.gc_heap.read_payload(current, |body| {
            assert!(body.slots()[0].watched, "unchanged string watch survives")
        });
        let accessor = object::get_own_symbol_descriptor(current, &vm.gc_heap, key).unwrap();
        assert!(matches!(
            accessor.kind,
            DescriptorKind::Accessor {
                getter: None,
                setter: None
            }
        ));
        assert!(!accessor.configurable() && accessor.enumerable());
        let private = vm.escape_scoped(private).as_symbol(&vm.gc_heap).unwrap();
        let descriptor = object::get_own_symbol_descriptor(current, &vm.gc_heap, private).unwrap();
        assert!(descriptor.writable() && descriptor.enumerable() && descriptor.configurable());
        vm.gc_heap.release_bytes(reserved);
    });
}
