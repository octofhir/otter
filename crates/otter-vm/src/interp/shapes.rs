//! Shape-transition helpers and property definition slow paths.
//!
//! # Contents
//! `shape_root`/`shape_child` (with rooted object/value transitions),
//! data-property stores (`set_property`, `ordinary_set_data_property`),
//! partial descriptor definition, freeze/seal, and shape-from-slots rebuild.
//!
//! # Invariants
//! Shape children are interned via the shape runtime; allocating a
//! child must root any live object value passed alongside it.
#![allow(unused_imports)]
use crate::*;

impl Interpreter {
    /// Empty GC-managed hidden-class root.
    #[must_use]
    pub(crate) fn shape_root(&self) -> object::ShapeHandle {
        self.shape_runtime.root()
    }

    /// Return the GC-managed child shape for appending `key` to `parent`.
    #[cfg(test)]
    pub(crate) fn shape_child(
        &mut self,
        parent: object::ShapeHandle,
        key: &str,
    ) -> Result<object::ShapeHandle, VmError> {
        let roots = self.collect_runtime_roots_without_shape_runtime();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
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
        value: &Value,
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
        let roots = self.collect_runtime_roots_without_shape_runtime();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            let p = obj as *mut object::JsObject as *mut RawGc;
            visitor(p);
            value.trace_value_slots(visitor);
        };
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
        descriptor: &object::PropertyDescriptor,
    ) -> Result<object::ShapeHandle, VmError> {
        let flags = descriptor.flags;
        let is_accessor = matches!(descriptor.kind, object::DescriptorKind::Accessor { .. });
        if let Some(child) =
            self.shape_runtime
                .child_if_cached(&self.gc_heap, parent, key, flags, is_accessor)
        {
            return Ok(child);
        }
        let roots = self.collect_runtime_roots_without_shape_runtime();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            let p = obj as *mut object::JsObject as *mut RawGc;
            visitor(p);
            match &descriptor.kind {
                object::DescriptorKind::Data { value } => value.trace_value_slots(visitor),
                object::DescriptorKind::Accessor { getter, setter } => {
                    if let Some(getter) = getter {
                        getter.trace_value_slots(visitor);
                    }
                    if let Some(setter) = setter {
                        setter.trace_value_slots(visitor);
                    }
                }
            }
        };
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
        let shape = object::shape(obj, &self.gc_heap);
        !shape.is_null()
            && object::is_extensible(obj, &self.gc_heap)
            && matches!(
                object::lookup_own(obj, &self.gc_heap, key),
                object::PropertyLookup::Absent
            )
            && self.shape_offset_of(shape, key).is_none()
    }

    pub(crate) fn update_array_prototype_length_after_index_store(
        &mut self,
        mut obj: object::JsObject,
        key: &str,
    ) {
        let Some(index) = object::array_index_property_name(key) else {
            return;
        };
        // An indexed property on a realm prototype becomes visible through
        // every ordinary dense array's holes, so the element fast paths that
        // answer a hole as `undefined` have to stop taking their shortcut.
        // Assignment reaches the object through this shape-advancing store
        // rather than through `define_own_property`, so the latch is tripped
        // from both places.
        if self.realm_intrinsics.array_prototype == Some(obj)
            || self.realm_intrinsics.object_prototype == Some(obj)
        {
            self.activate_array_index_accessor_protector();
        }
        if self.realm_intrinsics.array_prototype != Some(obj) {
            return;
        }
        let new_len = f64::from(index) + 1.0;
        let current = object::get(obj, &self.gc_heap, "length")
            .and_then(|value| value.as_number())
            .map(|number| number.as_f64())
            .unwrap_or(0.0);
        if new_len > current {
            object::set(
                &mut obj,
                &mut self.gc_heap,
                "length",
                Value::number(NumberValue::from_f64(new_len)),
            );
        }
    }

    /// Descriptor-aware data assignment that advances the object's GC-managed
    /// hidden class when a new own data property is created.
    pub(crate) fn ordinary_set_data_property(
        &mut self,
        mut obj: object::JsObject,
        key: &str,
        value: Value,
    ) -> Result<bool, VmError> {
        let shape = object::shape(obj, &self.gc_heap);
        // Past the fast-property cap, stop extending the transition
        // chain and let `object::ordinary_set_data_property` normalize
        // the object to dictionary storage (shape → null). Otherwise a
        // growing chain makes every lookup O(n) and bulk addition
        // O(n²).
        let old_count = object::shape_property_count(shape, &self.gc_heap) as usize;
        let should_add_shape =
            self.should_add_property(obj, key) && (old_count as u32) < object::MAX_FAST_PROPERTIES;
        let next_shape = if should_add_shape {
            Some(self.shape_child_rooting_object_value(shape, key, &mut obj, &value)?)
        } else {
            None
        };

        let ok = if let Some(next_shape) = next_shape {
            object::ordinary_set_data_property_with_shape(
                obj,
                &mut self.gc_heap,
                key,
                value,
                next_shape,
                old_count,
            )
        } else {
            object::ordinary_set_data_property(obj, &mut self.gc_heap, key, value)
        };
        if ok {
            self.update_array_prototype_length_after_index_store(obj, key);
        }
        Ok(ok)
    }

    /// Construction-time data store that advances the object's GC-managed
    /// hidden class when a new own data property is created.
    pub(crate) fn set_property(
        &mut self,
        mut obj: object::JsObject,
        key: &str,
        value: Value,
    ) -> Result<(), VmError> {
        let shape = object::shape(obj, &self.gc_heap);
        let old_count = object::shape_property_count(shape, &self.gc_heap) as usize;
        let should_add_shape =
            self.should_add_property(obj, key) && (old_count as u32) < object::MAX_FAST_PROPERTIES;
        let next_shape = if should_add_shape {
            Some(self.shape_child_rooting_object_value(shape, key, &mut obj, &value)?)
        } else {
            None
        };

        if let Some(next_shape) = next_shape {
            object::set_with_shape(obj, &mut self.gc_heap, key, value, next_shape, old_count);
        } else {
            object::set(&mut obj, &mut self.gc_heap, key, value);
        }
        self.update_array_prototype_length_after_index_store(obj, key);
        Ok(())
    }

    /// Field-presence-aware defineProperty path that advances the object's
    /// GC-managed hidden class when a new own property is created.
    pub(crate) fn define_own_property_partial(
        &mut self,
        mut obj: object::JsObject,
        key: &str,
        descriptor: object::PartialPropertyDescriptor,
    ) -> Result<bool, VmError> {
        let completed = descriptor.complete_for_new_property();
        let shape = object::shape(obj, &self.gc_heap);
        // Append a brand-new own property: extend the transition chain.
        if self.should_add_property(obj, key)
            && object::shape_property_count(shape, &self.gc_heap) < object::MAX_FAST_PROPERTIES
        {
            let next_shape =
                self.shape_child_rooting_object_descriptor(shape, key, &mut obj, &completed)?;
            return Ok(object::define_own_property_partial_with_shape(
                obj,
                &mut self.gc_heap,
                key,
                descriptor,
                next_shape,
            ));
        }
        // Redefine an existing slot on a shaped object: rebuild the hidden
        // class with the merged attributes so the shape keeps recording them
        // instead of flagging a per-object override.
        if !shape.is_null()
            && let Some((flags, is_accessor, offset)) =
                object::redefine_merged_attrs(obj, &self.gc_heap, key, &descriptor)
        {
            let mut ordered = object::shape_ordered_slot_attrs(&self.gc_heap, shape);
            if let Some(slot) = ordered.get_mut(offset as usize) {
                slot.1 = flags;
                slot.2 = is_accessor;
            }
            let redefine_shape = {
                let mut root_descriptor = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    if let Some(value) = descriptor.value.as_ref() {
                        value.trace_value_slots(visitor);
                    }
                    if let Some(getter) = descriptor.get.as_ref() {
                        getter.trace_value_slots(visitor);
                    }
                    if let Some(setter) = descriptor.set.as_ref() {
                        setter.trace_value_slots(visitor);
                    }
                };
                self.rebuild_shape_from_slots(&mut obj, &ordered, &mut root_descriptor)?
            };
            return Ok(object::define_own_property_partial_with_shape(
                obj,
                &mut self.gc_heap,
                key,
                descriptor,
                redefine_shape,
            ));
        }
        Ok(object::define_own_property_partial(
            obj,
            &mut self.gc_heap,
            key,
            descriptor,
        ))
    }

    /// Replay `ordered` `(key, flags, is_accessor)` slots from the empty root,
    /// returning the attribute-encoding hidden class they describe. The replay
    /// reuses shared transitions, so objects modified the same way (frozen,
    /// sealed, redefined) converge on one class and keep ICs monomorphic.
    /// `obj` is rooted across every transition allocation.
    pub(crate) fn rebuild_shape_from_slots(
        &mut self,
        obj: &mut object::JsObject,
        ordered: &[(String, object::PropertyFlags, bool)],
        extra_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<object::ShapeHandle, VmError> {
        let roots = self.collect_runtime_roots_without_shape_runtime();
        let mut shape = self.shape_runtime.root();
        for (key, flags, is_accessor) in ordered {
            let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                for &slot in &roots {
                    visitor(slot);
                }
                let p = obj as *mut object::JsObject as *mut RawGc;
                visitor(p);
                extra_visit(visitor);
            };
            shape = self
                .shape_runtime
                .child_with_roots(
                    &mut self.gc_heap,
                    shape,
                    key,
                    *flags,
                    *is_accessor,
                    &mut external_visit,
                )
                .map_err(VmError::from)?;
        }
        Ok(shape)
    }

    /// `Object.freeze` core: for a shaped object, transition to the
    /// attribute-encoding class recording every data slot as
    /// non-writable/non-configurable and accessor slots as non-configurable;
    /// dictionary-mode objects fall back to the in-place path.
    pub(crate) fn freeze_object(&mut self, mut obj: object::JsObject) -> Result<(), VmError> {
        let shape = object::shape(obj, &self.gc_heap);
        if shape.is_null() {
            object::freeze(obj, &mut self.gc_heap);
            return Ok(());
        }
        let mut ordered = object::shape_ordered_slot_attrs(&self.gc_heap, shape);
        for (_, flags, is_accessor) in ordered.iter_mut() {
            *flags = flags.with_configurable(false);
            if !*is_accessor {
                *flags = flags.with_writable(false);
            }
        }
        let mut no_extra = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let new_shape = self.rebuild_shape_from_slots(&mut obj, &ordered, &mut no_extra)?;
        object::freeze_with_shape(obj, &mut self.gc_heap, new_shape);
        Ok(())
    }

    /// `Object.seal` core: for a shaped object, transition to the
    /// attribute-encoding class recording every slot as non-configurable;
    /// dictionary-mode objects fall back to the in-place path.
    pub(crate) fn seal_object(&mut self, mut obj: object::JsObject) -> Result<(), VmError> {
        let shape = object::shape(obj, &self.gc_heap);
        if shape.is_null() {
            object::seal(obj, &mut self.gc_heap);
            return Ok(());
        }
        let mut ordered = object::shape_ordered_slot_attrs(&self.gc_heap, shape);
        for (_, flags, _) in ordered.iter_mut() {
            *flags = flags.with_configurable(false);
        }
        let mut no_extra = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let new_shape = self.rebuild_shape_from_slots(&mut obj, &ordered, &mut no_extra)?;
        object::seal_with_shape(obj, &mut self.gc_heap, new_shape);
        Ok(())
    }

    /// Look up a property slot in a GC-managed hidden-class shape.
    #[must_use]
    pub(crate) fn shape_offset_of(&mut self, shape: object::ShapeHandle, key: &str) -> Option<u32> {
        self.shape_runtime.offset_of(&self.gc_heap, shape, key)
    }
}
