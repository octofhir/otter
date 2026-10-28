//! Source-realm ownership of object and array literal allocation.
//!
//! # Contents
//! - Representation-neutral literal allocation selected by exact source fid.
//! - Active-realm cell creation shared with deopt materialization.
//! - Allocation-site placement: each literal instruction's feedback cell names
//!   one isolate site, registered on its first allocation.
//!
//! # Invariants
//! Interpreter and compiled/inlined instructions use the same source-function
//! realm. A realm switch precedes allocation and is restored on completion.
//! Owned array elements are copied before collection and rooted by the existing
//! allocator; native frames remain published throughout the slow transition.
//! A tracking site tags its young cells, a tenured site allocates old, and a
//! site the collector declined allocates plain young cells. Placement never
//! changes a literal's realm, shape or element representation.
//!
//! # See also
//! `allocation_ops` decodes bytecode operands; `runtime_activation::semantic_source`
//! resolves inlined source ownership without materializing a frame.

use std::sync::atomic::{AtomicU16, Ordering};

use otter_gc::SitePlacement;
use otter_gc::pretenuring::PretenureDecision;

use crate::{Interpreter, Value, VmError};

impl Interpreter {
    /// The isolate site of one literal instruction's cell, registered on its
    /// first use; zero without a cell or once every id is taken.
    fn literal_site(&mut self, cell: Option<&AtomicU16>) -> u16 {
        let Some(cell) = cell else {
            return 0;
        };
        let sites = self.gc_heap.allocation_sites_mut();
        let site = cell.load(Ordering::Relaxed);
        if sites.contains(site) {
            return site;
        }
        let site = sites.register();
        cell.store(site, Ordering::Relaxed);
        site
    }

    /// Placement of one literal allocation through its site cell: the site's
    /// current decision, registering the site on its first allocation.
    pub(crate) fn literal_site_placement(&mut self, cell: Option<&AtomicU16>) -> SitePlacement {
        let site = self.literal_site(cell);
        if site == 0 {
            return SitePlacement::Young(0);
        }
        match self.gc_heap.allocation_sites().decision(site) {
            PretenureDecision::Tenure => SitePlacement::Old,
            PretenureDecision::DontTenure => SitePlacement::Young(0),
            PretenureDecision::Undecided | PretenureDecision::MaybeTenure => {
                SitePlacement::Young(site)
            }
        }
    }

    /// The site one generation bakes for a literal instruction's cell: the
    /// one its first allocation registered, else none.
    pub(crate) fn registered_jit_allocation_site(
        &self,
        cell: Option<&AtomicU16>,
    ) -> crate::jit::JitAllocationSite {
        let id = cell.map_or(0, |cell| cell.load(Ordering::Relaxed));
        if id == 0 || !self.gc_heap.allocation_sites().contains(id) {
            return crate::jit::JitAllocationSite::default();
        }
        let sites = self.gc_heap.allocation_sites();
        crate::jit::JitAllocationSite {
            id,
            record: sites.record_address(id),
            mode: match sites.decision(id) {
                PretenureDecision::Tenure => crate::jit::JitSiteMode::Old,
                PretenureDecision::DontTenure => crate::jit::JitSiteMode::Young,
                PretenureDecision::Undecided | PretenureDecision::MaybeTenure => {
                    crate::jit::JitSiteMode::Track
                }
            },
        }
    }

    pub(crate) fn allocate_static_object_literal(
        &mut self,
        context: &crate::ExecutionContext,
        function_id: u32,
        first_key: u32,
        values: &[Value],
        placement: SitePlacement,
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
                vm.allocate_object_with_layout(layout, &mut values, placement)
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
        placement: SitePlacement,
    ) -> Result<Value, VmError>
    where
        I: IntoIterator<Item = Value>,
    {
        let elements: Vec<Value> = elements.into_iter().collect();
        // An empty literal is a bare shell the generated tiers never attribute.
        let placement = if elements.is_empty() {
            SitePlacement::Young(0)
        } else {
            placement
        };
        if let Some(realm) = self.foreign_function_realm(function_id) {
            return self.with_host_realm_id(realm, move |vm| {
                vm.allocate_array_in_active_realm(elements, placement)
            });
        }
        self.allocate_array_in_active_realm(elements, placement)
    }

    /// The shared slab of the array-literal boilerplate constant `idx` in the
    /// chunk that owns `function_id`, built once: its strings are allocated
    /// old, so the slab never holds a young edge and is never written.
    pub(crate) fn array_boilerplate_slab(
        &mut self,
        context: &crate::ExecutionContext,
        function_id: u32,
        idx: u32,
    ) -> Result<crate::array::elements::ElementSlabHandle, VmError> {
        use otter_bytecode::BoilerplateElement;
        let (key, elements) = context
            .array_boilerplate_for_function(function_id, idx)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(slab) = self.array_boilerplates().get(&key) {
            return Ok(*slab);
        }
        let _roots = self.scope_runtime_roots_guard();
        let mut values: Vec<Value> = Vec::with_capacity(elements.len());
        for element in elements {
            let value = match element {
                BoilerplateElement::Number { bits } => {
                    Value::number(crate::number::NumberValue::from_f64(f64::from_bits(*bits)))
                }
                BoilerplateElement::String { utf16 } => {
                    let made = &values;
                    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                        for value in made {
                            value.trace_value_slots(visitor);
                        }
                    };
                    let string = crate::string::gc_body::alloc_old_string_body_with_roots(
                        &mut self.gc_heap,
                        utf16,
                        &mut visit,
                    )?;
                    Value::string_gc(string)
                }
                BoilerplateElement::Boolean { value } => Value::boolean(*value),
                BoilerplateElement::Null => Value::null(),
                BoilerplateElement::Undefined => Value::undefined(),
                BoilerplateElement::Hole => Value::hole(),
            };
            values.push(value);
        }
        let kind = crate::array::boilerplate_kind(&values);
        let made = &values;
        let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for value in made {
                value.trace_value_slots(visitor);
            }
        };
        let slab = crate::array::elements::alloc_shared_slab(
            &mut self.gc_heap,
            &values,
            kind,
            &mut visit,
        )?;
        self.array_boilerplates_mut().insert(key, slab);
        Ok(slab)
    }

    /// A fresh array of the boilerplate constant `idx`, sharing its slab.
    pub(crate) fn allocate_array_boilerplate_value(
        &mut self,
        context: &crate::ExecutionContext,
        function_id: u32,
        idx: u32,
        placement: SitePlacement,
    ) -> Result<Value, VmError> {
        let slab = self.array_boilerplate_slab(context, function_id, idx)?;
        let allocate = move |vm: &mut Self| {
            let _roots = vm.scope_runtime_roots_guard();
            let prototype = vm.current_array_prototype_override();
            let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                if let Some(prototype) = &prototype {
                    prototype.trace_value_slots(visitor);
                }
            };
            let array =
                crate::array::from_shared_slab(&mut vm.gc_heap, slab, placement, &mut visit)?;
            Ok(Value::array(vm.register_array_prototype_override(array)?))
        };
        match self.foreign_function_realm(function_id) {
            Some(realm) => self.with_host_realm_id(realm, allocate),
            None => allocate(self),
        }
    }

    pub(crate) fn allocate_object_in_active_realm(&mut self) -> Result<Value, VmError> {
        let prototype = self.object_prototype_object_opt();
        let root = self.object_root(
            prototype,
            crate::object::DEFAULT_INLINE_CAPACITY,
            crate::object::ShapeState::ORDINARY,
        )?;
        self.allocate_empty_object(root)
    }

    /// An empty object whose `[[Prototype]]` is `null`, in any realm.
    pub(crate) fn allocate_null_prototype_object(&mut self) -> Result<Value, VmError> {
        let root = self.null_prototype_root();
        self.allocate_empty_object(root)
    }

    fn allocate_empty_object(
        &mut self,
        root: crate::object::ShapeHandle,
    ) -> Result<Value, VmError> {
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
        placement: SitePlacement,
    ) -> Result<Value, VmError>
    where
        I: IntoIterator<Item = Value>,
    {
        let array =
            self.alloc_runtime_rooted_array_at_site(elements, placement, &mut [], &mut [])?;
        Ok(Value::array(array))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_sites_register_once_tag_tracking_births_and_follow_decisions() {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        vm.gc_heap.set_gc_stress(0, false);
        let cell = AtomicU16::new(0);
        let placement = vm.literal_site_placement(Some(&cell));
        let site = cell.load(Ordering::Relaxed);
        assert_ne!(site, 0);
        assert_eq!(placement, SitePlacement::Young(site));
        assert_eq!(
            vm.literal_site_placement(Some(&cell)),
            placement,
            "one site"
        );
        assert_eq!(vm.literal_site_placement(None), SitePlacement::Young(0));
        let header = |value: Value| value.as_array().expect("array literal").as_header_ptr();
        let young = vm
            .allocate_array_literal_value(0, [Value::number_i32(1)], placement)
            .expect("tracked birth");
        // SAFETY: the array was just allocated and no collection ran since.
        unsafe {
            assert!((*header(young)).is_young());
            assert_eq!((*header(young)).allocation_site(), site);
        }
        let tracked = vm.registered_jit_allocation_site(Some(&cell));
        assert_eq!(tracked.id, site);
        assert_eq!(tracked.mode, crate::jit::JitSiteMode::Track);
        assert_eq!(
            tracked.record,
            vm.gc_heap.allocation_sites().record_address(site)
        );

        vm.gc_heap
            .allocation_sites_mut()
            .set_decision_for_test(site, PretenureDecision::Tenure);
        assert_eq!(vm.literal_site_placement(Some(&cell)), SitePlacement::Old);
        assert_eq!(
            vm.registered_jit_allocation_site(Some(&cell)).mode,
            crate::jit::JitSiteMode::Old
        );
        let old = vm
            .allocate_array_literal_value(0, [Value::number_i32(2)], SitePlacement::Old)
            .expect("tenured birth");
        // SAFETY: the array was just allocated and no collection ran since.
        unsafe {
            assert!(!(*header(old)).is_young());
            assert_eq!((*header(old)).allocation_site(), 0);
        }
        let empty = vm
            .allocate_array_literal_value(0, [], SitePlacement::Old)
            .expect("empty shell");
        // SAFETY: as above.
        unsafe { assert!((*header(empty)).is_young(), "empty shells stay untracked") };

        let declined = AtomicU16::new(0);
        vm.literal_site_placement(Some(&declined));
        let other = declined.load(Ordering::Relaxed);
        vm.gc_heap
            .allocation_sites_mut()
            .set_decision_for_test(other, PretenureDecision::DontTenure);
        assert_eq!(
            vm.literal_site_placement(Some(&declined)),
            SitePlacement::Young(0)
        );
        assert_eq!(
            vm.registered_jit_allocation_site(Some(&declined)).mode,
            crate::jit::JitSiteMode::Young
        );
    }

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
            assert!(!vm.object_literal_layouts().contains_key(&(function_id, 0)));
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
                let result = vm.allocate_static_object_literal(
                    &context,
                    function_id,
                    0,
                    &values,
                    SitePlacement::Young(0),
                )?;
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
                        let layout = vm.object_literal_layouts()[&(function_id, 0)];
                        assert_eq!(layout.len(), COUNT);
                        let shape = vm
                            .shape_runtime
                            .handle_for_id(&vm.gc_heap, layout.shape_id())
                            .unwrap();
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
                    .allocate_array_literal_value(42, [], SitePlacement::Young(0))
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
                    let default =
                        vm.allocate_array_literal_value(0, [], SitePlacement::Young(0))?;
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
