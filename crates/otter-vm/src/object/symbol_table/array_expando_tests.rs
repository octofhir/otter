//! Exact Array expando symbol-table growth under actual cap collection.
//!
//! # Contents
//! - Capacity 0/4/64 pending children and symbol descriptions through growth.
//! - Atomic OOM refusal or reclaimed capacity admission for a full table.
//! - Existing symbol overwrites at full heap cap without allocation.
//!
//! # Invariants
//! - Only the Array handle retains its bag between scoped construction steps.
//! - Ordinary object descriptor publication is the sole symbol mutation owner.
//! - Motion assertions compare the same children across actual table collection.
//! - Failure preserves the physical source table, order, attributes and values.
//!
//! # See also
//! - [`super`] owns managed symbol records and rooted table growth.
//! - [`crate::array::expando`] owns the Array storage projection.

use crate::array;
use crate::object::{self, ShapeHandle, ShapeState, shape_body};
use crate::symbol::JsSymbol;
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
    // "array-key" is one sequential Latin-1 string and one old symbol identity.
    otter_gc::GcHeap::cell_bytes::<crate::string::JsStringBody>("array-key".len()) as u64
        + cell_bytes::<crate::symbol::SymbolBody>()
}

#[test]
fn full_bag_symbol_growth_moves_exact_edges_and_rejects_or_commits_atomically() {
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
            vm.with_handle_scope(|vm, inner| {
                vm.ensure_array_expando(inner, owner).expect("rooted bag birth");
            });
            let old_keys = [symbol(vm, scope), symbol(vm, scope)];
            let mut current = array::expando_object(vm.escape_scoped(owner).as_array().unwrap(), &vm.gc_heap).unwrap();
            for (index, key) in old_keys.iter().copied().enumerate() {
                let key = vm.escape_scoped(key).as_symbol(&vm.gc_heap).unwrap();
                assert!(
                    object::ordinary_set_symbol_data_property(
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
            current = array::expando_object(vm.escape_scoped(owner).as_array().unwrap(), &vm.gc_heap).unwrap();
            let source_table = vm.gc_heap.read_payload(current, |body| {
                body.exotic().expect("ordinary bag symbol sidecar").symbol_props
            });
            vm.gc_heap.read_payload(source_table, |table| {
                assert_eq!(table.capacity(), 2);
                assert_eq!(table.entries().len(), 2);
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
            current = array::expando_object(vm.escape_scoped(owner).as_array().unwrap(), &vm.gc_heap).unwrap();
            for (index, key) in old_keys.iter().copied().enumerate() {
                let key = vm.escape_scoped(key).as_symbol(&vm.gc_heap).unwrap();
                let value = vm.escape_scoped(children[index]);
                assert!(
                    object::ordinary_set_symbol_data_property(&mut current, &mut vm.gc_heap, key, value)
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
            current = array::expando_object(vm.escape_scoped(owner).as_array().unwrap(), &vm.gc_heap).unwrap();
            let key = vm.escape_scoped(incoming).as_symbol(&vm.gc_heap).unwrap();
            let pending = vm.escape_scoped(children[2]);
            let result =
                object::ordinary_set_symbol_data_property(&mut current, &mut vm.gc_heap, key, pending);
            assert!(
                vm.gc_heap.gc_cycle_counts().1 > cycles.1,
                "actual table allocation owns collection"
            );
            let current_array = vm.escape_scoped(owner).as_array().unwrap();
            assert_eq!(Some(current), array::expando_object(current_array, &vm.gc_heap));
            assert_eq!(vm.escape_scoped(owner), vm.escape_scoped(alias));
            assert_eq!(array::len(current_array, &vm.gc_heap), 0);
            assert!(vm.gc_heap.read_payload(current_array, |body| body.exotic.is_null()));
            let current_table = vm.gc_heap.read_payload(current, |body| {
                body.exotic().expect("retained ordinary bag symbol sidecar").symbol_props
            });
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
            assert_eq!(object::own_symbol_key_values(current, &vm.gc_heap).len(), expected_len);
            for (index, old_key) in old_keys.iter().copied().enumerate() {
                let old_key = vm.escape_scoped(old_key).as_symbol(&vm.gc_heap).unwrap();
                let actual = object::own_symbol_key_values(current, &vm.gc_heap)[index];
                assert!(actual.as_symbol(&vm.gc_heap).unwrap().ptr_eq(old_key));
                let descriptor = object::get_own_symbol_descriptor(current, &vm.gc_heap, old_key).unwrap();
                assert!(
                    descriptor.writable() && descriptor.enumerable() && descriptor.configurable()
                );
                assert_eq!(
                    object::get_own_symbol(current, &vm.gc_heap, old_key),
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
                assert!(object::own_symbol_key_values(current, &vm.gc_heap)[2].as_symbol(&vm.gc_heap).unwrap().ptr_eq(key));
                assert_eq!(
                    object::get_own_symbol(current, &vm.gc_heap, key),
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
                assert!(object::get_own_symbol_descriptor(current, &vm.gc_heap, key).is_none());
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
                object::ordinary_set_symbol_data_property(&mut current, &mut vm.gc_heap, first, value)
                    .unwrap()
            );
            assert_eq!(vm.gc_heap.gc_cycle_counts(), cycles);
            assert_eq!(
                object::get_own_symbol(current, &vm.gc_heap, first),
                Some(value)
            );
            assert_eq!(
                vm.gc_heap.read_payload(current, |body| {
                    body.exotic().expect("ordinary bag symbol sidecar").symbol_props
                }),
                current_table
            );
            assert_eq!(object::own_symbol_key_values(current, &vm.gc_heap).len(), expected_len);
            assert!(object::own_symbol_key_values(current, &vm.gc_heap)[0].as_symbol(&vm.gc_heap).unwrap().ptr_eq(first));
            vm.gc_heap.release_bytes(extra);
            vm.gc_heap.release_bytes(reserve);
            assert_eq!(vm.gc_heap.stats().reserved_bytes, 0);
        });
    }
}
