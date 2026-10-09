//! Shape-transition helpers and property definition slow paths.
//!
//! # Contents
//! `shape_root`/`shape_child` (with rooted object/value transitions),
//! data-property creation (`create_data_property`) and ordinary assignment,
//! partial descriptor definition, freeze/seal, and shape-from-slots rebuild.
//!
//! # Invariants
//! Shape children are interned via the shape runtime; allocating a
//! child must root any live object value passed alongside it.
//!
//! # See also
//! `object::prototype_validity` for dependencies on prepared prototype chains.
#![allow(unused_imports)]
use crate::rooting::RootScopeExt;
use crate::*;

impl Interpreter {
    /// Return the GC-managed child shape for appending `key` to `parent`.
    #[cfg(test)]
    pub(crate) fn shape_child(
        &mut self,
        parent: object::ShapeHandle,
        key: &str,
    ) -> Result<object::ShapeHandle, VmError> {
        let _runtime_roots = self.scope_runtime_roots_guard();
        let mut parent = parent;
        let mut pending = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: the parent slot precedes this registration and survives
        // collecting allocation through the complete child lookup.
        unsafe { pending.add_raw_slot(std::ptr::addr_of_mut!(parent).cast::<RawGc>()) };
        let mut external_visit = |_: &mut dyn FnMut(*mut RawGc)| {};
        self.shape_runtime
            .child_with_roots(
                &mut self.gc_heap,
                parent,
                key,
                object::PropertyFlags::data_default(),
                false,
                &mut external_visit,
            )
            .map_err(VmError::from)
    }

    pub(crate) fn shape_child_rooting_object_value(
        &mut self,
        parent: object::ShapeHandle,
        key: &str,
        obj: &mut object::JsObject,
        value: &mut Value,
    ) -> Result<object::ShapeHandle, VmError> {
        // Fast path: a previously seen field-shape transition resolves with no
        // allocation, so it needs no rooting. Building an object whose layout
        // already exists (every object after the first of its class) lands
        // here and skips the full runtime-root walk below.
        if let Some(child) = self.shape_runtime.child_if_cached(
            &self.gc_heap,
            parent,
            key,
            object::PropertyFlags::data_default(),
            false,
        ) {
            return Ok(child);
        }
        let _runtime_roots = self.scope_runtime_roots_guard();
        let mut parent = parent;
        let mut pending = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: caller receiver/value slots and this parent slot remain
        // stationary across lookup, allocation and return to their owner.
        unsafe {
            pending.add_object(obj);
            pending.add_value(value);
            pending.add_raw_slot(std::ptr::addr_of_mut!(parent).cast::<RawGc>());
        }
        let mut external_visit = |_: &mut dyn FnMut(*mut RawGc)| {};
        self.shape_runtime
            .child_with_roots(
                &mut self.gc_heap,
                parent,
                key,
                object::PropertyFlags::data_default(),
                false,
                &mut external_visit,
            )
            .map_err(VmError::from)
    }

    pub(crate) fn shape_child_rooting_object_descriptor(
        &mut self,
        parent: object::ShapeHandle,
        key: &str,
        obj: &mut object::JsObject,
        flags: object::PropertyFlags,
        is_accessor: bool,
    ) -> Result<object::ShapeHandle, VmError> {
        if let Some(child) =
            self.shape_runtime
                .child_if_cached(&self.gc_heap, parent, key, flags, is_accessor)
        {
            return Ok(child);
        }
        let _runtime_roots = self.scope_runtime_roots_guard();
        let mut parent = parent;
        let mut pending = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: these slots survive every collecting transition allocation;
        // the caller anchors the descriptor's values in the handle arena.
        unsafe {
            pending.add_object(obj);
            pending.add_raw_slot(std::ptr::addr_of_mut!(parent).cast::<RawGc>());
        }
        let mut external_visit = |_: &mut dyn FnMut(*mut RawGc)| {};
        self.shape_runtime
            .child_with_roots(
                &mut self.gc_heap,
                parent,
                key,
                flags,
                is_accessor,
                &mut external_visit,
            )
            .map_err(VmError::from)
    }

    pub(crate) fn should_add_property(&mut self, obj: object::JsObject, key: &str) -> bool {
        // A shaped object's own string keys are exactly its shape's.
        let shape = object::shape(obj, &self.gc_heap);
        !object::shape_body::is_dictionary_of(shape)
            && object::is_extensible(obj, &self.gc_heap)
            && self.shape_offset_of(shape, key).is_none()
    }

    pub(crate) fn update_array_prototype_length_after_index_store(
        &mut self,
        mut obj: object::JsObject,
        key: &str,
    ) -> Result<(), VmError> {
        let Some(index) = object::array_index_property_name(key) else {
            return Ok(());
        };
        // An indexed property on a realm prototype becomes visible through
        // every ordinary dense array's holes, so the element fast paths that
        // answer a hole as `undefined` have to stop taking their shortcut.
        // Assignment reaches the object through this shape-advancing store
        // rather than through `define_own_property`, so the latch is tripped
        // from both places.
        if self.realm_intrinsics.array_prototype() == Some(obj)
            || self.realm_intrinsics.object_prototype() == Some(obj)
        {
            self.activate_array_index_accessor_protector();
        }
        if self.realm_intrinsics.array_prototype() != Some(obj) {
            return Ok(());
        }
        let new_len = f64::from(index) + 1.0;
        let current = object::get(obj, &self.gc_heap, "length")
            .and_then(|value| value.as_number())
            .map(|number| number.as_f64())
            .unwrap_or(0.0);
        if new_len > current {
            if !object::ordinary_set_data_property(
                &mut obj,
                &mut self.gc_heap,
                "length",
                Value::number(NumberValue::from_f64(new_len)),
            )? {
                return Err(VmError::TypeMismatch);
            }
        }
        Ok(())
    }

    /// Descriptor-aware data assignment that advances the object's GC-managed
    /// hidden class when a new own data property is created.
    pub(crate) fn ordinary_set_data_property(
        &mut self,
        mut obj: object::JsObject,
        key: &str,
        mut value: Value,
    ) -> Result<bool, VmError> {
        let _runtime_roots = self.scope_runtime_roots_guard();
        let shape = object::shape(obj, &self.gc_heap);
        // Past the fast-property cap, stop extending the transition
        // chain and let `object::ordinary_set_data_property` normalize
        // the object to its immutable dictionary shape. Otherwise a growing chain makes every lookup O(n) and bulk addition
        // O(n²).
        let old_count = object::shape_property_count(shape, &self.gc_heap) as usize;
        let should_add_shape =
            self.should_add_property(obj, key) && (old_count as u32) < object::MAX_FAST_PROPERTIES;
        let next_shape = if should_add_shape {
            Some(self.shape_child_rooting_object_value(shape, key, &mut obj, &mut value)?)
        } else {
            None
        };

        let ok = if let Some(next_shape) = next_shape {
            object::ordinary_set_data_property_with_shape(
                &mut obj,
                &mut self.gc_heap,
                key,
                value,
                next_shape,
            )?
        } else {
            object::ordinary_set_data_property(&mut obj, &mut self.gc_heap, key, value)?
        };
        if ok {
            self.update_array_prototype_length_after_index_store(obj, key)?;
        }
        Ok(ok)
    }

    /// Construction-time data store that advances the object's GC-managed
    /// hidden class when a new own data property is created.
    pub(crate) fn create_data_property(
        &mut self,
        obj: &mut object::JsObject,
        key: &str,
        mut value: Value,
    ) -> Result<(), VmError> {
        let _runtime_roots = self.scope_runtime_roots_guard();
        let shape = object::shape(*obj, &self.gc_heap);
        let old_count = object::shape_property_count(shape, &self.gc_heap) as usize;
        let should_add_shape =
            self.should_add_property(*obj, key) && (old_count as u32) < object::MAX_FAST_PROPERTIES;
        let next_shape = if should_add_shape {
            Some(self.shape_child_rooting_object_value(shape, key, obj, &mut value)?)
        } else {
            None
        };

        let descriptor = object::PartialPropertyDescriptor::from_full(
            &object::PropertyDescriptor::data(value, true, true, true),
        );
        let accepted = if let Some(next_shape) = next_shape {
            object::define_own_property_partial_with_shape(
                obj,
                &mut self.gc_heap,
                key,
                descriptor,
                next_shape,
            )?
        } else {
            object::define_own_property_partial(obj, &mut self.gc_heap, key, descriptor)?
        };
        if !accepted {
            return Err(VmError::TypeMismatch);
        }
        self.update_array_prototype_length_after_index_store(*obj, key)?;
        Ok(())
    }

    /// Field-presence-aware defineProperty path that advances the object's
    /// GC-managed hidden class when a new own property is created.
    pub(crate) fn define_own_property_partial(
        &mut self,
        obj_ref: &mut object::JsObject,
        key: &str,
        descriptor: object::PartialPropertyDescriptor,
    ) -> Result<bool, VmError> {
        let completed = descriptor.complete_for_new_property();
        let shape = object::shape(*obj_ref, &self.gc_heap);
        // Append a brand-new own property: extend the transition chain. The
        // shape-child computation allocates, so the descriptor's payload
        // values ride anchor slots across it.
        if self.should_add_property(*obj_ref, key)
            && object::shape_property_count(shape, &self.gc_heap) < object::MAX_FAST_PROPERTIES
        {
            let flags = completed.flags;
            let is_accessor = matches!(completed.kind, object::DescriptorKind::Accessor { .. });
            let (next_shape, descriptor) = self.with_descriptor_anchored(descriptor, |this| {
                this.shape_child_rooting_object_descriptor(shape, key, obj_ref, flags, is_accessor)
            })?;
            return object::define_own_property_partial_with_shape(
                obj_ref,
                &mut self.gc_heap,
                key,
                descriptor,
                next_shape,
            )
            .map_err(VmError::from);
        }
        // Redefine an existing slot on a shaped object: rebuild the hidden
        // class with the merged attributes. The final shape remains the sole
        // ordinary descriptor owner.
        if !object::shape_body::is_dictionary_of(shape)
            && let Some((flags, is_accessor, offset)) =
                object::redefine_merged_attrs(*obj_ref, &self.gc_heap, key, &descriptor)
        {
            let mut ordered = object::shape_ordered_slot_attrs(&self.gc_heap, shape);
            if let Some(slot) = ordered.get_mut(offset as usize) {
                slot.1 = flags;
                slot.2 = is_accessor;
            }
            // The shape rebuild allocates; the descriptor's payload values
            // ride anchor slots (a trace through shared references into the
            // stack-local descriptor is not a root the collector can
            // rewrite).
            let (redefine_shape, descriptor) =
                self.with_descriptor_anchored(descriptor, |this| {
                    let mut no_extra_roots = |_: &mut dyn FnMut(*mut RawGc)| {};
                    let state = object::state(*obj_ref, &this.gc_heap).with_dictionary(false);
                    this.rebuild_shape_from_slots(obj_ref, &ordered, state, &mut no_extra_roots)
                })?;
            return object::define_own_property_partial_with_shape(
                obj_ref,
                &mut self.gc_heap,
                key,
                descriptor,
                redefine_shape,
            )
            .map_err(VmError::from);
        }
        object::define_own_property_partial(obj_ref, &mut self.gc_heap, key, descriptor)
            .map_err(VmError::from)
    }

    /// Give a dictionary-mode object — and every dictionary-mode object on its
    /// prototype chain — the hidden class describing the slots it already has.
    ///
    /// Bootstrap installers build namespace objects (`Math`, `JSON`, `Reflect`,
    /// `Atomics`, `Intl`, …) through the heap-only define path, which has no
    /// shape runtime to take a transition from and so leaves them in dictionary
    /// storage for life. Nothing can be cached on such an object: an inline
    /// cache names its receiver by hidden class, and a dictionary object has
    /// none. Migrating on the first cache attach puts them back on the ordinary
    /// shaped path, where a validity cell can cover the complete chain.
    ///
    /// Handles root the receiver and traversal cursor across allocations.
    /// The walk has the same cycle/depth safety bound as prototype lookup.
    pub(crate) fn migrate_slow_to_fast(&mut self, obj: &mut object::JsObject) {
        self.with_handle_scope(|interp, scope| {
            let receiver = interp.scoped_value(scope, Value::object(*obj));
            let cursor = interp.scoped_value(scope, Value::object(*obj));
            for _ in 0..object::PROTO_CHAIN_HARD_CAP {
                // A function or other non-ordinary object in the chain keeps
                // its own property storage; the dictionary walk ends there.
                let Some(mut current) = interp.escape_scoped(cursor).as_object() else {
                    break;
                };
                if let Some(ordered) =
                    object::dictionary_ordered_slot_attrs(current, &interp.gc_heap)
                {
                    let mut no_extra_roots = |_: &mut dyn FnMut(*mut RawGc)| {};
                    let state = object::state(current, &interp.gc_heap).with_dictionary(false);
                    let Ok(shape) = interp.rebuild_shape_from_slots(
                        &mut current,
                        &ordered,
                        state,
                        &mut no_extra_roots,
                    ) else {
                        break;
                    };
                    object::adopt_fast_shape(current, &mut interp.gc_heap, shape);
                }
                interp
                    .shape_runtime
                    .register_shape(&interp.gc_heap, object::shape(current, &interp.gc_heap));
                let Some(next) = object::prototype(current, &interp.gc_heap) else {
                    break;
                };
                interp.set_scoped(cursor, Value::object(next));
            }
            *obj = interp.escape_scoped(receiver).as_object().unwrap();
        });
    }

    /// Replay `ordered` `(key, flags, is_accessor)` slots from the root of
    /// `obj`'s lineage, returning the attribute-encoding hidden class they
    /// describe. The replay reuses shared transitions, so objects modified the
    /// same way (frozen, sealed, redefined) converge on one class and keep ICs
    /// monomorphic. `obj` is rooted across every transition allocation.
    pub(crate) fn rebuild_shape_from_slots(
        &mut self,
        obj: &mut object::JsObject,
        ordered: &[(String, object::PropertyFlags, bool)],
        state: object::ShapeState,
        extra_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<object::ShapeHandle, VmError> {
        use crate::rooting::RootScopeExt;
        let _runtime_roots = self.scope_runtime_roots_guard();
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: the receiver slot is caller-owned and stationary throughout
        // state preparation and replay, and outlives this registration.
        unsafe {
            roots.add_object(obj);
        }
        let current = object::shape(*obj, &self.gc_heap);
        let root = object::shape_body::lineage_root_of(&self.gc_heap, current);
        let object_slot = (obj as *mut object::JsObject).cast::<RawGc>();
        let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            visitor(object_slot);
            extra_visit(visitor);
        };
        let root = self.shape_runtime.state_with_roots(
            &mut self.gc_heap,
            root,
            state.with_dictionary(false),
            &mut visit,
        )?;
        self.replay_slots_from(root, obj, ordered, extra_visit)
    }

    /// Replay `ordered` slots onto `shape` (a lineage root), rooting `obj`.
    pub(crate) fn replay_slots_from(
        &mut self,
        shape: object::ShapeHandle,
        obj: &mut object::JsObject,
        ordered: &[(String, object::PropertyFlags, bool)],
        extra_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<object::ShapeHandle, VmError> {
        let _runtime_roots = self.scope_runtime_roots_guard();
        // SAFETY: the heap arena outlives replay; no Local escapes. Holding the
        // current partial root closes the key-interning collection window.
        let scope = unsafe { otter_gc::HandleScope::from_ptr(self.gc_heap.handle_stack_ptr()) };
        let mut current = scope.local(shape);
        for (key, flags, is_accessor) in ordered {
            let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                let p = obj as *mut object::JsObject as *mut RawGc;
                visitor(p);
                extra_visit(visitor);
            };
            let next = self
                .shape_runtime
                .child_with_roots(
                    &mut self.gc_heap,
                    current.get(),
                    key,
                    *flags,
                    *is_accessor,
                    &mut external_visit,
                )
                .map_err(VmError::from)?;
            current = scope.local(next);
        }
        Ok(current.get())
    }

    /// `Object.freeze` core: for a shaped object, transition to the
    /// attribute-encoding class recording every data slot as
    /// non-writable/non-configurable and accessor slots as non-configurable;
    /// dictionary-mode objects fall back to the in-place path.
    pub(crate) fn freeze_object(&mut self, obj: object::JsObject) -> Result<(), VmError> {
        self.with_handle_scope(|this, scope| {
            let owner = this.scoped_value(scope, Value::object(obj));
            let mut obj = this
                .escape_scoped(owner)
                .as_object()
                .expect("rooted object");
            let shape = object::shape(obj, &this.gc_heap);
            if object::shape_body::is_dictionary_of(shape) {
                object::freeze(&mut obj, &mut this.gc_heap)?;
                return Ok(());
            }
            let mut ordered = object::shape_ordered_slot_attrs(&this.gc_heap, shape);
            for (_, flags, is_accessor) in &mut ordered {
                *flags = flags.with_configurable(false);
                if !*is_accessor {
                    *flags = flags.with_writable(false);
                }
            }
            let target = object::shape_body::state_of(shape).with_extensible(false);
            let new_shape =
                this.rebuild_shape_from_slots(&mut obj, &ordered, target, &mut |_| {})?;
            object::freeze_with_shape(obj, &mut this.gc_heap, new_shape);
            Ok(())
        })
    }

    /// `Object.seal` core: for a shaped object, transition to the
    /// attribute-encoding class recording every slot as non-configurable;
    /// dictionary-mode objects fall back to the in-place path.
    pub(crate) fn seal_object(&mut self, obj: object::JsObject) -> Result<(), VmError> {
        self.with_handle_scope(|this, scope| {
            let owner = this.scoped_value(scope, Value::object(obj));
            let mut obj = this
                .escape_scoped(owner)
                .as_object()
                .expect("rooted object");
            let shape = object::shape(obj, &this.gc_heap);
            if object::shape_body::is_dictionary_of(shape) {
                object::seal(&mut obj, &mut this.gc_heap)?;
                return Ok(());
            }
            let mut ordered = object::shape_ordered_slot_attrs(&this.gc_heap, shape);
            for (_, flags, is_accessor) in &mut ordered {
                *flags = flags.with_configurable(false);
                let _ = is_accessor;
            }
            let target = object::shape_body::state_of(shape).with_extensible(false);
            let new_shape =
                this.rebuild_shape_from_slots(&mut obj, &ordered, target, &mut |_| {})?;
            object::seal_with_shape(obj, &mut this.gc_heap, new_shape);
            Ok(())
        })
    }

    /// Look up a property slot in a GC-managed hidden-class shape. A spelling
    /// nothing has interned names no shape key.
    #[must_use]
    pub(crate) fn shape_offset_of(&self, shape: object::ShapeHandle, key: &str) -> Option<u32> {
        let atom = self.shape_runtime.names().lookup(key);
        if atom == crate::property_atom::AtomId::NONE {
            return None;
        }
        self.shape_slot_of_atom(shape, atom).map(|slot| slot.offset)
    }

    /// The slot `shape` gives `atom`, through the isolate's lookup cache
    /// (V8's `DescriptorLookupCache`) before the transition-chain walk.
    #[must_use]
    pub(crate) fn shape_slot_of_atom(
        &self,
        shape: object::ShapeHandle,
        atom: crate::property_atom::AtomId,
    ) -> Option<object::shape_body::ShapeSlot> {
        let heap = &self.gc_heap;
        self.own_slot_cache
            .slot(object::shape_body::id_of(shape), atom, || {
                object::shape_body::shape_slot_of_atom(heap, shape, atom)
            })
    }
}
