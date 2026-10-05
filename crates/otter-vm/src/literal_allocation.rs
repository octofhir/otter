//! Source-realm ownership of object and array literal allocation.
//!
//! # Contents
//! - Representation-neutral literal allocation selected by exact source fid.
//! - Active-realm cell creation shared with deopt materialization.
//!
//! # Invariants
//! Interpreter and compiled/inlined instructions use the same source-function
//! realm. A realm switch precedes allocation and is restored on completion.
//! Owned array elements are copied before collection and rooted by the existing
//! allocator; native frames remain published throughout the slow transition.
//!
//! # See also
//! `allocation_ops` decodes bytecode operands; `runtime_activation::semantic_source`
//! resolves inlined source ownership without materializing a frame.

use crate::{Interpreter, Value, VmError};

impl Interpreter {
    pub(crate) fn allocate_static_object_literal(
        &mut self,
        context: &crate::ExecutionContext,
        function_id: u32,
        first_key: u32,
        values: &[Value],
    ) -> Result<Value, VmError> {
        self.with_handle_scope(|vm, scope| {
            let handles = values
                .iter()
                .map(|value| vm.scoped_value(scope, *value))
                .collect::<Vec<_>>();
            let allocate = |vm: &mut Self| {
                let layout =
                    vm.object_literal_layout(context, function_id, first_key, handles.len())?;
                let mut values = handles
                    .iter()
                    .map(|local| vm.handle_arena.get(local.index()))
                    .collect::<Vec<_>>();
                vm.allocate_object_with_layout(layout, &mut values)
            };
            match vm.foreign_function_realm(function_id) {
                Some(realm) => vm.with_host_realm_id(realm, allocate),
                None => allocate(vm),
            }
        })
    }

    pub(crate) fn allocate_object_literal_value(
        &mut self,
        function_id: u32,
    ) -> Result<Value, VmError> {
        if let Some(realm) = self.foreign_function_realm(function_id) {
            return self.with_host_realm_id(realm, Self::allocate_object_in_active_realm);
        }
        self.allocate_object_in_active_realm()
    }

    pub(crate) fn allocate_array_literal_value<I>(
        &mut self,
        function_id: u32,
        elements: I,
    ) -> Result<Value, VmError>
    where
        I: IntoIterator<Item = Value>,
    {
        let elements: Vec<Value> = elements.into_iter().collect();
        if let Some(realm) = self.foreign_function_realm(function_id) {
            return self
                .with_host_realm_id(realm, move |vm| vm.allocate_array_in_active_realm(elements));
        }
        self.allocate_array_in_active_realm(elements)
    }

    pub(crate) fn allocate_object_in_active_realm(&mut self) -> Result<Value, VmError> {
        let prototype = self.object_prototype_object_opt();
        let root = self.object_root(
            prototype,
            crate::object::DEFAULT_INLINE_CAPACITY,
            crate::object::ShapeState::ORDINARY,
        )?;
        let object =
            match crate::object::try_alloc_object_with_shape_no_collect(&mut self.gc_heap, root) {
                Some(object) => object,
                None => {
                    let _roots = self.scope_runtime_roots_guard();
                    crate::object::alloc_object_with_shape_roots(
                        &mut self.gc_heap,
                        root,
                        &mut |_: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {},
                    )?
                }
            };
        Ok(Value::object(object))
    }

    pub(crate) fn allocate_array_in_active_realm<I>(
        &mut self,
        elements: I,
    ) -> Result<Value, VmError>
    where
        I: IntoIterator<Item = Value>,
    {
        let array = self.alloc_runtime_rooted_array_from_values(elements, &[], &[])?;
        Ok(Value::array(array))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LiteralChild {
        marker: u64,
    }

    impl otter_gc::SafeTraceable for LiteralChild {
        const TYPE_TAG: u8 = 0xe9;

        fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {}
    }

    #[test]
    fn static_literal_first_layout_roots_inputs_and_uses_source_realm_at_every_stress_stride() {
        use otter_bytecode::Constant;

        // Layout planning does not collect or advance the eligible young
        // stress counter. Prime it with actual allocations so the object's
        // allocation collects after its first uncached layout at every stride.
        const COUNT: usize = 64;
        for stride in 1..=16 {
            let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
            let realm = vm.create_host_realm().expect("source realm");
            let mut module = crate::test_support::minimal_bytecode_module("source-literal.js");
            module.constants = (0..COUNT)
                .map(|index| Constant::String {
                    utf16: format!("sourceField{index}").encode_utf16().collect(),
                })
                .collect();
            let context = vm
                .link_module(module, crate::source_registry::SourceRegistry::default())
                .expect("literal key context");
            let function_id = context.function_base();
            vm.function_realm_ids.insert(function_id, realm.0);
            assert!(!vm.object_literal_layouts.contains_key(&(function_id, 0)));
            vm.with_runtime_roots(|vm| {
                // The only pending child roots across the first literal call
                // are the handles installed by allocate_static_object_literal.
                vm.gc_heap.set_gc_stress(0, false);
                let child = vm.gc_heap.alloc(LiteralChild { marker: 731 })?;
                let original = child.offset();
                let values = vec![Value::from_other_gc(child.raw()); COUNT];
                vm.gc_heap.set_gc_stress(stride, false);
                let before_minor = vm.gc_heap.gc_stats().minor_gc_cycles;
                for _ in 1..stride {
                    vm.gc_heap.alloc(LiteralChild { marker: 0 })?;
                }
                assert_eq!(
                    vm.gc_heap.gc_stats().minor_gc_cycles,
                    before_minor,
                    "priming must leave the pending input unmoved"
                );
                let before_updates = vm.gc_heap.gc_stats().minor_slot_updates;
                let result =
                    vm.allocate_static_object_literal(&context, function_id, 0, &values)?;
                assert!(
                    vm.gc_heap.gc_stats().minor_gc_cycles > before_minor,
                    "stride {stride} must evacuate pending young inputs inside the literal call"
                );
                assert!(
                    vm.gc_heap.gc_stats().minor_slot_updates > before_updates,
                    "stride {stride} must rewrite the pending input roots"
                );
                assert_eq!(vm.active_realm_id, 0, "restore caller realm");
                vm.with_handle_scope(|vm, scope| {
                    let result = vm.scoped_value(scope, result);
                    vm.gc_heap.set_gc_stress(0, false);
                    vm.with_host_realm(realm, |vm| {
                        let object = vm.handle_arena.get(result.index()).as_object().unwrap();
                        assert_eq!(
                            crate::object::prototype(object, &vm.gc_heap),
                            vm.object_prototype_object_opt()
                        );
                        let first = crate::object::layout_slot(object, &vm.gc_heap, 0);
                        let moved = first
                            .as_raw_gc()
                            .and_then(|raw| raw.checked_cast::<LiteralChild>())
                            .expect("input remains the exact child type");
                        assert_ne!(moved.offset(), original, "pending input really relocated");
                        assert_eq!(vm.gc_heap.read_payload(moved, |body| body.marker), 731);
                        for index in 0..COUNT {
                            assert_eq!(
                                crate::object::layout_slot(object, &vm.gc_heap, index),
                                first,
                                "alias at slot {index}, stride {stride}"
                            );
                        }
                        let layout = vm.object_literal_layouts[&(function_id, 0)];
                        assert_eq!(layout.len(), COUNT);
                        let shape = vm.shape_runtime.handle_for_id(layout.shape_id()).unwrap();
                        assert_eq!(crate::object::shape_body::inline_capacity_of(shape), 64);
                        Ok(())
                    })?;
                    // Once initialized, the object graph alone keeps the
                    // aliased child live through another full tracing pass.
                    vm.collect_minor_tracing_runtime_roots();
                    vm.gc_heap
                        .collect_full(&mut |_| {})
                        .expect("full collection");
                    let object = vm.handle_arena.get(result.index()).as_object().unwrap();
                    let last = crate::object::layout_slot(object, &vm.gc_heap, COUNT - 1);
                    let child = last
                        .as_raw_gc()
                        .and_then(|raw| raw.checked_cast::<LiteralChild>())
                        .expect("retained final field child");
                    assert_eq!(vm.gc_heap.read_payload(child, |body| body.marker), 731);
                    Ok::<_, VmError>(())
                })
            })
            .expect("source literal allocation under moving collection");
        }
    }

    #[test]
    fn empty_array_extra_realm_sidecar_returns_relocated_shell() {
        let mut vm = Interpreter::with_string_heap_cap(32 * 1024 * 1024)
            .expect("fixture interpreter bootstrap");
        let realm = vm.create_host_realm().expect("extra realm");
        vm.with_runtime_roots(|vm| {
            vm.with_host_realm(realm, |vm| {
                // Drive the collecting boundary by its real cap, independent
                // of inherited stress stride; keep dead cells until that call.
                vm.gc_heap.set_gc_stress(0, false);
                // Dead cells provide recoverable space when the actual sidecar
                // allocation crosses the configured cap and collects.
                for _ in 0..64 {
                    vm.allocate_object_in_active_realm()?;
                }
                let array = crate::array::alloc_array_with_roots(&mut vm.gc_heap, &mut |_| {})?;
                // Retiring the LAB refunds its unused reservation without
                // collecting the pending shell or dead recoverable cells.
                vm.gc_heap.set_gc_stress(0, false);
                let old_offset = array.offset();
                let before = vm.gc_heap.gc_stats().minor_gc_cycles;
                let reserve = vm.gc_heap.max_heap_bytes() - vm.gc_heap.tracked_bytes() - 1;
                vm.gc_heap.reserve_bytes_no_collect(reserve)?;
                let array = vm.register_array_prototype_override(array)?;
                vm.gc_heap.release_bytes(reserve);
                assert_ne!(
                    array.offset(),
                    old_offset,
                    "actual sidecar cap collection must relocate the shell"
                );
                assert!(vm.gc_heap.gc_stats().minor_gc_cycles > before);
                assert_eq!(crate::array::len(array, &vm.gc_heap), 0);
                assert_eq!(
                    crate::array::prototype_override(array, &vm.gc_heap),
                    vm.current_array_prototype_override()
                );
                Ok(())
            })
        })
        .expect("rooted extra-realm sidecar allocation");
    }

    #[test]
    fn empty_literals_select_source_realm_and_restore_active_realm() {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let realm = vm.create_host_realm().expect("extra realm");
        vm.function_realm_ids.insert(42, realm.0);
        vm.with_runtime_roots(|vm| {
            vm.with_handle_scope(|vm, scope| {
                let object = vm
                    .allocate_object_literal_value(42)
                    .expect("foreign-source object");
                let object = vm.scoped_value(scope, object);
                let array = vm
                    .allocate_array_literal_value(42, [])
                    .expect("foreign-source array");
                let array = vm.scoped_value(scope, array);
                assert_eq!(vm.active_realm_id, 0);
                vm.with_host_realm(realm, |vm| {
                    let object = vm
                        .handle_arena
                        .get(object.index())
                        .as_object()
                        .expect("rooted object");
                    let array = vm
                        .handle_arena
                        .get(array.index())
                        .as_array()
                        .expect("rooted array");
                    assert_eq!(
                        crate::object::prototype(object, &vm.gc_heap),
                        vm.object_prototype_object_opt()
                    );
                    assert_eq!(
                        crate::array::prototype_override(array, &vm.gc_heap),
                        vm.current_array_prototype_override()
                    );
                    let default = vm.allocate_array_literal_value(0, [])?;
                    assert!(
                        crate::array::prototype_override(default.as_array().unwrap(), &vm.gc_heap)
                            .is_none()
                    );
                    assert_eq!(
                        vm.active_realm_id, realm.0,
                        "nested allocation restores calling realm"
                    );
                    Ok(())
                })
                .expect("check source prototypes");
            });
        });
        assert_eq!(vm.active_realm_id, 0);
    }
}
