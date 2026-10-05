//! Exact array symbol descriptors and actual preparation collection proofs.
//!
//! # Contents
//! - Attribute, kind, order, integrity and rejection semantics.
//! - Capacity 0/4/64 children survive actual sidecar/table cap collection.
//! - Full managed tables reject insertion atomically; overwrite cannot allocate.
//!
//! # Invariants
//! - Fixtures allocate through the canonical interpreter handle scope.
//! - Every motion assertion compares the same live child before and after GC.
//! - A sidecar allocation refusal publishes no sidecar; table growth failure
//!   preserves the preexisting sidecar, table, ordered keys and descriptors.
//!
//! # See also
//! - [`super`] owns one descriptor table and noncollecting publication.

use super::*;
use crate::object::{self, ShapeHandle, ShapeState, shape_body};
use crate::{Interpreter, Local, Value};
use otter_gc::SafeTraceable;

struct Reclaimable([u64; 16384]);
impl SafeTraceable for Reclaimable {
    const TYPE_TAG: u8 = 0xeb;
    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        let _ = self.0[0];
    }
}

fn symbol<'s>(vm: &mut Interpreter, scope: &'s crate::handles::HandleScope) -> Local<'s> {
    let description = vm.scoped_string(scope, "array-key").expect("description");
    let description = vm
        .escape_scoped(description)
        .as_string(&vm.gc_heap)
        .unwrap();
    let symbol = JsSymbol::new(&mut vm.gc_heap, Some(description)).expect("symbol");
    vm.scoped_value(scope, Value::symbol(symbol))
}

#[test]
fn symbol_attributes_kind_order_rejection_and_integrity_use_one_descriptor() {
    let mut vm = Interpreter::new().expect("fixture interpreter");
    vm.with_handle_scope(|vm, scope| {
        let owner = vm.scoped_array(scope, 0).expect("array");
        let keys: Vec<_> = (0..3).map(|_| symbol(vm, scope)).collect();
        let mut current = vm.escape_scoped(owner).as_array().unwrap();
        let first = vm.escape_scoped(keys[0]).as_symbol(&vm.gc_heap).unwrap();
        assert!(
            define_symbol_property_partial(
                &mut current,
                &mut vm.gc_heap,
                first,
                PartialPropertyDescriptor::from_full(&PropertyDescriptor::data(
                    Value::number_i32(11),
                    false,
                    false,
                    true,
                )),
            )
            .unwrap()
        );
        assert_eq!(Value::array(current), vm.escape_scoped(owner));
        assert!(
            !ordinary_set_symbol_data_property(
                &mut current,
                &mut vm.gc_heap,
                first,
                Value::number_i32(12),
            )
            .unwrap()
        );
        let second = vm.escape_scoped(keys[1]).as_symbol(&vm.gc_heap).unwrap();
        assert!(
            define_symbol_property_partial(
                &mut current,
                &mut vm.gc_heap,
                second,
                PartialPropertyDescriptor::from_full(&PropertyDescriptor::accessor(
                    None, None, true, true,
                )),
            )
            .unwrap()
        );
        assert!(
            define_symbol_property_partial(
                &mut current,
                &mut vm.gc_heap,
                first,
                PartialPropertyDescriptor::from_full(&PropertyDescriptor::accessor(
                    None, None, false, true,
                )),
            )
            .unwrap()
        );
        assert!(
            own_symbol_keys(current, &vm.gc_heap)
                .into_iter()
                .zip([first, second])
                .all(|(actual, expected)| actual.ptr_eq(expected))
        );
        assert_eq!(own_symbol_keys(current, &vm.gc_heap).len(), 2);
        assert!(
            get_symbol_descriptor(current, &vm.gc_heap, first)
                .unwrap()
                .is_accessor()
        );
        assert_eq!(
            get_symbol_accessor(current, &vm.gc_heap, first),
            Some((None, None))
        );
        assert_eq!(get_symbol_property(current, &vm.gc_heap, first), None);
        assert!(
            !ordinary_set_symbol_data_property(
                &mut current,
                &mut vm.gc_heap,
                first,
                Value::number_i32(23),
            )
            .unwrap()
        );
        // Ordinary definition can replace a configurable accessor. Assignment
        // cannot; the replacement retains the original creation position.
        assert!(
            define_symbol_property_partial(
                &mut current,
                &mut vm.gc_heap,
                first,
                PartialPropertyDescriptor::from_full(&PropertyDescriptor::data(
                    Value::number_i32(23),
                    true,
                    false,
                    true,
                )),
            )
            .unwrap()
        );
        crate::array::prevent_extensions(current, &mut vm.gc_heap);
        let missing = vm.escape_scoped(keys[2]).as_symbol(&vm.gc_heap).unwrap();
        assert!(
            !ordinary_set_symbol_data_property(
                &mut current,
                &mut vm.gc_heap,
                missing,
                Value::number_i32(24),
            )
            .unwrap()
        );
        assert!(
            ordinary_set_symbol_data_property(
                &mut current,
                &mut vm.gc_heap,
                first,
                Value::number_i32(25),
            )
            .unwrap()
        );
        crate::array::set_integrity_level(current, &mut vm.gc_heap, false);
        assert!(crate::array::test_integrity_level(
            current,
            &vm.gc_heap,
            false
        ));
        assert!(!crate::array::test_integrity_level(
            current,
            &vm.gc_heap,
            true
        ));
        assert!(!delete_symbol_property(current, &mut vm.gc_heap, first));
        crate::array::set_integrity_level(current, &mut vm.gc_heap, true);
        assert!(crate::array::test_integrity_level(
            current,
            &vm.gc_heap,
            true
        ));
        assert!(
            !ordinary_set_symbol_data_property(
                &mut current,
                &mut vm.gc_heap,
                first,
                Value::number_i32(26),
            )
            .unwrap()
        );
        let descriptor = get_symbol_descriptor(current, &vm.gc_heap, first).unwrap();
        assert!(!descriptor.writable());
        assert!(!descriptor.enumerable());
        assert!(!descriptor.configurable());
        assert_eq!(
            get_symbol_property(current, &vm.gc_heap, first),
            Some(Value::number_i32(25))
        );
        assert!(
            own_symbol_keys(current, &vm.gc_heap)
                .into_iter()
                .zip([first, second])
                .all(|(actual, expected)| actual.ptr_eq(expected))
        );
        assert_eq!(own_symbol_keys(current, &vm.gc_heap).len(), 2);
    });
}

#[test]
fn private_names_do_not_enumerate_or_receive_public_integrity_attributes() {
    let mut vm = Interpreter::new().expect("fixture interpreter");
    vm.with_handle_scope(|vm, scope| {
        let owner = vm.scoped_array(scope, 0).expect("array");
        let private = JsSymbol::new_private(&mut vm.gc_heap, None).expect("private name");
        let private = vm.scoped_value(scope, Value::symbol(private));
        let mut current = vm.escape_scoped(owner).as_array().unwrap();
        let key = vm.escape_scoped(private).as_symbol(&vm.gc_heap).unwrap();
        assert!(
            ordinary_set_symbol_data_property(
                &mut current,
                &mut vm.gc_heap,
                key,
                Value::number_i32(37),
            )
            .unwrap()
        );
        assert!(own_symbol_keys(current, &vm.gc_heap).is_empty());
        crate::array::set_integrity_level(current, &mut vm.gc_heap, true);
        assert!(crate::array::test_integrity_level(
            current,
            &vm.gc_heap,
            true
        ));
        assert!(
            get_symbol_descriptor(current, &vm.gc_heap, key)
                .unwrap()
                .writable()
        );
        assert!(
            ordinary_set_symbol_data_property(
                &mut current,
                &mut vm.gc_heap,
                key,
                Value::number_i32(38),
            )
            .unwrap()
        );
    });
}

fn cell_bytes<T>() -> u64 {
    (std::mem::size_of::<otter_gc::GcHeader>() + std::mem::size_of::<T>())
        .next_multiple_of(otter_gc::OBJECT_ALIGNMENT) as u64
}

fn child_shape(vm: &mut Interpreter, capacity: usize) -> ShapeHandle {
    let root = vm
        .shape_runtime
        .new_root(
            &mut vm.gc_heap,
            shape_body::ShapePrototype::Null,
            capacity,
            ShapeHandle::null(),
            ShapeState::ORDINARY,
            &mut |_| {},
        )
        .expect("exact child root");
    vm.shape_child(root, "marker").expect("exact child field")
}

fn child_bytes(capacity: usize) -> u64 {
    let geometry = object::FieldLayout::current();
    geometry.cell_bytes(capacity) as u64
        + if capacity == 0 {
            u64::from(geometry.slab_words_byte) + object::FieldLocation::words_bytes(1) as u64
        } else {
            0
        }
}

fn symbol_bytes() -> u64 {
    // "array-key" is one inline Latin-1 string and one old symbol identity.
    cell_bytes::<crate::string::JsStringBody>() + cell_bytes::<crate::symbol::SymbolBody>()
}

#[test]
fn symbol_sidecar_refusal_rewrites_exact_pending_receiver_child_and_description() {
    for capacity in [0, 4, 64] {
        let cap = 4 * 1024 * 1024;
        let mut vm = Interpreter::with_string_heap_cap(cap).expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let shape = child_shape(vm, capacity);
            let keeper =
                object::alloc_object_with_shape_roots(&mut vm.gc_heap, shape, &mut |_| {}).unwrap();
            let _keeper = vm.scoped_value(scope, Value::object(keeper));
            vm.force_gc()
                .expect("settle retained source shape before fresh inputs");
            let input_bytes =
                cell_bytes::<crate::array::ArrayBody>() + child_bytes(capacity) + symbol_bytes();
            let reserve = cap - vm.gc_heap.stats().allocated_bytes as u64 - input_bytes - 1;
            vm.gc_heap
                .reserve_bytes_with_roots(reserve, &mut |_| {})
                .expect("admit exact physical inputs before fresh nursery cells");
            let admitted = vm.gc_heap.stats();
            let cycles = vm.gc_heap.gc_cycle_counts();
            let owner = vm.scoped_array(scope, 0).unwrap();
            let alias = vm.scoped_value(scope, vm.escape_scoped(owner));
            let child =
                object::alloc_object_with_shape_roots(&mut vm.gc_heap, shape, &mut |_| {}).unwrap();
            let child = vm.scoped_value(scope, Value::object(child));
            let child_alias = vm.scoped_value(scope, vm.escape_scoped(child));
            let marker = vm.scoped_value(scope, Value::number_i32(831 + capacity as i32));
            vm.scoped_set_slot(scope, child, 0, marker).unwrap();
            let key = symbol(vm, scope);
            let before = [
                vm.escape_scoped(owner).as_array().unwrap().offset(),
                vm.escape_scoped(child).as_object().unwrap().offset(),
            ];
            let before_description = vm
                .escape_scoped(key)
                .as_symbol(&vm.gc_heap)
                .unwrap()
                .description()
                .unwrap()
                .handle()
                .offset();
            assert_eq!(
                vm.gc_heap.gc_cycle_counts(),
                cycles,
                "fresh inputs never collect in setup"
            );
            assert_eq!(
                vm.gc_heap.stats().allocated_bytes - admitted.allocated_bytes,
                input_bytes as usize
            );
            assert_eq!(vm.gc_heap.tracked_bytes(), cap - 1);
            let mut current = vm.escape_scoped(owner).as_array().unwrap();
            assert!(
                vm.gc_heap
                    .read_payload(current, |body| body.exotic.is_null())
            );
            let current_key = vm.escape_scoped(key).as_symbol(&vm.gc_heap).unwrap();
            let pending = vm.escape_scoped(child);
            let error = ordinary_set_symbol_data_property(
                &mut current,
                &mut vm.gc_heap,
                current_key,
                pending,
            )
            .expect_err("the missing sidecar cannot fit in the actual one-byte remainder");
            vm.gc_heap.release_bytes(reserve);
            assert!(error.requested_bytes() > 0);
            assert_eq!(error.heap_limit_bytes(), cap);
            assert!(vm.gc_heap.gc_cycle_counts().1 > cycles.1);
            assert_eq!(Value::array(current), vm.escape_scoped(owner));
            assert_eq!(vm.escape_scoped(owner), vm.escape_scoped(alias));
            assert_eq!(vm.escape_scoped(child), vm.escape_scoped(child_alias));
            assert_ne!(current.offset(), before[0]);
            let child = vm.escape_scoped(child).as_object().unwrap();
            assert_ne!(child.offset(), before[1]);
            assert_eq!(
                vm.gc_heap
                    .read_payload(child, |body| body.inline_capacity()),
                capacity
            );
            assert_eq!(
                object::get(child, &vm.gc_heap, "marker"),
                Some(Value::number_i32(831 + capacity as i32))
            );
            let key = vm.escape_scoped(key).as_symbol(&vm.gc_heap).unwrap();
            assert_ne!(
                key.description().unwrap().handle().offset(),
                before_description
            );
            assert_eq!(
                key.description().unwrap().to_lossy_string(&vm.gc_heap),
                "array-key"
            );
            assert!(get_symbol_descriptor(current, &vm.gc_heap, key).is_none());
            assert!(own_symbol_keys(current, &vm.gc_heap).is_empty());
            assert!(
                vm.gc_heap
                    .read_payload(current, |body| body.exotic.is_null())
            );
        });
    }
}

#[test]
fn full_managed_symbol_growth_moves_exact_old_edges_and_rejects_or_commits_atomically() {
    for (capacity, reclaimable) in [0, 4, 64]
        .into_iter()
        .flat_map(|capacity| [(capacity, false), (capacity, true)])
    {
        let cap = 4 * 1024 * 1024;
        let mut vm = Interpreter::with_string_heap_cap(cap).expect("fixture interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_handle_scope(|vm, scope| {
            let shape = child_shape(vm, capacity);
            let keeper =
                object::alloc_object_with_shape_roots(&mut vm.gc_heap, shape, &mut |_| {}).unwrap();
            let _keeper = vm.scoped_value(scope, Value::object(keeper));
            let owner = vm.scoped_array(scope, 0).unwrap();
            let alias = vm.scoped_value(scope, vm.escape_scoped(owner));
            let old_keys = [symbol(vm, scope), symbol(vm, scope)];
            let mut current = vm.escape_scoped(owner).as_array().unwrap();
            for (index, key) in old_keys.iter().copied().enumerate() {
                let key = vm.escape_scoped(key).as_symbol(&vm.gc_heap).unwrap();
                assert!(
                    ordinary_set_symbol_data_property(
                        &mut current,
                        &mut vm.gc_heap,
                        key,
                        Value::number_i32(index as i32)
                    )
                    .unwrap()
                );
            }
            vm.force_gc()
                .expect("settle actual full source table and shape");
            current = vm.escape_scoped(owner).as_array().unwrap();
            let source_sidecar = vm.gc_heap.read_payload(current, |body| body.exotic);
            let source_table = vm
                .gc_heap
                .read_payload(source_sidecar, |body| body.symbol_properties);
            vm.gc_heap.read_payload(source_table, |table| {
                assert_eq!(table.capacity(), 2);
                assert_eq!(table.descriptors().count(), 2);
            });
            let funding_bytes = if reclaimable {
                cell_bytes::<Reclaimable>()
            } else {
                0
            };
            let input_bytes = 3 * child_bytes(capacity) + symbol_bytes() + funding_bytes;
            // Birth the complete fresh input set while the settled heap has
            // actual headroom. Only then book exact pressure without collecting:
            // no source allocator may age these children before table growth.
            let admitted = vm.gc_heap.stats();
            let cycles = vm.gc_heap.gc_cycle_counts();
            let assert_input_phase = |vm: &Interpreter, phase: &str, expected_bytes: u64| {
                let observed = vm.gc_heap.stats();
                assert_eq!(
                    vm.gc_heap.gc_cycle_counts(),
                    cycles,
                    "capacity={capacity} reclaimable={reclaimable} phase={phase}: fresh input allocation must not collect; before={admitted:?} after={observed:?}"
                );
                assert_eq!(
                    observed.allocated_bytes - admitted.allocated_bytes,
                    expected_bytes as usize,
                    "capacity={capacity} reclaimable={reclaimable} phase={phase}: exact physical input charge"
                );
                assert_eq!(
                    observed.tracked_bytes,
                    admitted.tracked_bytes + expected_bytes,
                    "capacity={capacity} reclaimable={reclaimable} phase={phase}: actual cap ledger equals completed cells"
                );
            };
            let mut children = Vec::new();
            let mut child_aliases = Vec::new();
            for index in 0..3 {
                let child =
                    object::alloc_object_with_shape_roots(&mut vm.gc_heap, shape, &mut |_| {})
                        .expect("fresh current child before pressure");
                let child = vm.scoped_value(scope, Value::object(child));
                assert_input_phase(vm, "child body and suffix", (index as u64 + 1) * child_bytes(capacity));
                let child_alias = vm.scoped_value(scope, vm.escape_scoped(child));
                let marker =
                    vm.scoped_value(scope, Value::number_i32(937 + index + capacity as i32));
                vm.scoped_set_slot(scope, child, 0, marker).unwrap();
                assert_input_phase(vm, "existing marker store", (index as u64 + 1) * child_bytes(capacity));
                children.push(child);
                child_aliases.push(child_alias);
            }
            current = vm.escape_scoped(owner).as_array().unwrap();
            for (index, key) in old_keys.iter().copied().enumerate() {
                let key = vm.escape_scoped(key).as_symbol(&vm.gc_heap).unwrap();
                let value = vm.escape_scoped(children[index]);
                assert!(
                    ordinary_set_symbol_data_property(&mut current, &mut vm.gc_heap, key, value)
                        .unwrap()
                );
            }
            assert_input_phase(vm, "existing table overwrites", 3 * child_bytes(capacity));
            let incoming = symbol(vm, scope);
            assert_input_phase(vm, "incoming symbol and description", 3 * child_bytes(capacity) + symbol_bytes());
            if reclaimable {
                vm.gc_heap.alloc_old(Reclaimable([0; 16384])).unwrap();
            }
            assert_input_phase(vm, "optional dead LOS cell", input_bytes);
            let reserve = cap
                .checked_sub(vm.gc_heap.tracked_bytes())
                .and_then(|remaining| remaining.checked_sub(1))
                .expect("all fresh inputs fit below the unchanged actual cap");
            assert!(reserve > 0, "pressure must be a genuine additional reservation");
            vm.gc_heap
                .reserve_bytes_no_collect(reserve)
                .expect("book exact cap-minus-one pressure without aging fresh inputs");
            let before: Vec<_> = children
                .iter()
                .map(|child| vm.escape_scoped(*child).as_object().unwrap().offset())
                .collect();
            assert_ne!(before[0], before[1]);
            assert_ne!(before[1], before[2]);
            assert_ne!(before[0], before[2]);
            let before_description = vm
                .escape_scoped(incoming)
                .as_symbol(&vm.gc_heap)
                .unwrap()
                .description()
                .unwrap()
                .handle()
                .offset();
            assert_eq!(
                vm.gc_heap.gc_cycle_counts(),
                cycles,
                "fresh values/key stay young until table growth"
            );
            assert_eq!(
                vm.gc_heap.stats().allocated_bytes - admitted.allocated_bytes,
                input_bytes as usize
            );
            // The public tracked usage excludes an unused charged LAB tail.
            // Check the effective physical+reservation ledger, so a canonical
            // tail retirement cannot reveal room that would bypass collection.
            let pressured = vm.gc_heap.stats();
            assert_eq!(pressured.tracked_bytes, cap - 1);
            assert_eq!(
                pressured.tracked_bytes,
                pressured.allocated_bytes as u64 + pressured.reserved_bytes
            );
            let effective_headroom = cap - pressured.tracked_bytes;
            let minimum_growth_bytes = cell_bytes::<crate::object::symbol_table::SymbolPropsBody>();
            assert!(
                effective_headroom < minimum_growth_bytes,
                "actual four-record growth needs more than its fixed body; even that body cannot fit after refunding every unused LAB byte"
            );
            current = vm.escape_scoped(owner).as_array().unwrap();
            let key = vm.escape_scoped(incoming).as_symbol(&vm.gc_heap).unwrap();
            let pending = vm.escape_scoped(children[2]);
            let result =
                ordinary_set_symbol_data_property(&mut current, &mut vm.gc_heap, key, pending);
            assert!(
                vm.gc_heap.gc_cycle_counts().1 > cycles.1,
                "actual table allocation owns collection"
            );
            assert_eq!(Value::array(current), vm.escape_scoped(owner));
            assert_eq!(vm.escape_scoped(owner), vm.escape_scoped(alias));
            assert_eq!(crate::array::len(current, &vm.gc_heap), 0);
            assert_eq!(
                vm.gc_heap.read_payload(current, |body| body.exotic),
                source_sidecar
            );
            let current_table = vm
                .gc_heap
                .read_payload(source_sidecar, |body| body.symbol_properties);
            let key = vm.escape_scoped(incoming).as_symbol(&vm.gc_heap).unwrap();
            assert_ne!(
                key.description().unwrap().handle().offset(),
                before_description
            );
            assert_eq!(
                key.description().unwrap().to_lossy_string(&vm.gc_heap),
                "array-key"
            );
            let expected_len = if reclaimable { 3 } else { 2 };
            assert_eq!(own_symbol_keys(current, &vm.gc_heap).len(), expected_len);
            for (index, old_key) in old_keys.iter().copied().enumerate() {
                let old_key = vm.escape_scoped(old_key).as_symbol(&vm.gc_heap).unwrap();
                let actual = own_symbol_keys(current, &vm.gc_heap)[index];
                assert!(actual.ptr_eq(old_key));
                let descriptor = get_symbol_descriptor(current, &vm.gc_heap, old_key).unwrap();
                assert!(
                    descriptor.writable() && descriptor.enumerable() && descriptor.configurable()
                );
                assert_eq!(
                    get_symbol_property(current, &vm.gc_heap, old_key),
                    Some(vm.escape_scoped(children[index]))
                );
            }
            for (index, child) in children.iter().copied().enumerate() {
                assert_eq!(
                    vm.escape_scoped(child),
                    vm.escape_scoped(child_aliases[index])
                );
                let child = vm.escape_scoped(child).as_object().unwrap();
                assert_ne!(
                    child.offset(),
                    before[index],
                    "same fresh table or pending child moved"
                );
                assert_eq!(
                    vm.gc_heap
                        .read_payload(child, |body| body.inline_capacity()),
                    capacity
                );
                assert_eq!(
                    object::get(child, &vm.gc_heap, "marker"),
                    Some(Value::number_i32(937 + index as i32 + capacity as i32))
                );
            }
            if reclaimable {
                assert!(result.expect("reclaimed allocation funds managed table growth"));
                assert_ne!(current_table, source_table);
                vm.gc_heap
                    .read_payload(current_table, |table| assert_eq!(table.capacity(), 4));
                assert!(own_symbol_keys(current, &vm.gc_heap)[2].ptr_eq(key));
                assert_eq!(
                    get_symbol_property(current, &vm.gc_heap, key),
                    Some(vm.escape_scoped(children[2]))
                );
            } else {
                let error =
                    result.expect_err("full preexisting managed table growth preserves actual OOM");
                assert!(error.requested_bytes() > 0);
                assert_eq!(error.heap_limit_bytes(), cap);
                assert_eq!(current_table, source_table);
                vm.gc_heap
                    .read_payload(current_table, |table| assert_eq!(table.capacity(), 2));
                assert!(get_symbol_descriptor(current, &vm.gc_heap, key).is_none());
            }
            // A genuine full-cap overwrite changes only the live existing
            // value, preserving ordered key/attributes and physical table.
            let extra = cap - vm.gc_heap.tracked_bytes();
            vm.gc_heap.reserve_bytes_no_collect(extra).unwrap();
            let cycles = vm.gc_heap.gc_cycle_counts();
            let first = vm
                .escape_scoped(old_keys[0])
                .as_symbol(&vm.gc_heap)
                .unwrap();
            let value = vm.escape_scoped(children[2]);
            assert!(
                ordinary_set_symbol_data_property(&mut current, &mut vm.gc_heap, first, value)
                    .unwrap()
            );
            assert_eq!(vm.gc_heap.gc_cycle_counts(), cycles);
            assert_eq!(
                get_symbol_property(current, &vm.gc_heap, first),
                Some(value)
            );
            assert_eq!(
                vm.gc_heap
                    .read_payload(source_sidecar, |body| body.symbol_properties),
                current_table
            );
            assert_eq!(own_symbol_keys(current, &vm.gc_heap).len(), expected_len);
            assert!(own_symbol_keys(current, &vm.gc_heap)[0].ptr_eq(first));
            vm.gc_heap.release_bytes(extra);
            vm.gc_heap.release_bytes(reserve);
            assert_eq!(vm.gc_heap.stats().reserved_bytes, 0);
        });
    }
}
