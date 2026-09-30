//! Prototype-keyed hidden-class lineages: instance roots and prototype changes.
//!
//! # Contents
//! - [`Interpreter::instance_root`] / [`Interpreter::object_root`] — the root
//!   shape of objects created with a given `[[Prototype]]`.
//! - [`Interpreter::set_ordinary_prototype`] — §10.1.2.1
//!   OrdinarySetPrototypeOf for ordinary objects.
//!
//! # Invariants
//! - A shape fixes its objects' `[[Prototype]]` (V8 maps, JSC structures,
//!   SpiderMonkey shapes all do), so creating an object picks its prototype's
//!   root and changing a prototype changes the shape; nothing else stores it.
//! - An ordinary prototype caches its instances' root on itself (V8's
//!   `PrototypeInfo::ObjectCreateMap`), `null` uses the shape runtime's root,
//!   and a non-ordinary prototype gets a fresh root that lives as long as the
//!   objects built from it.
//! - Shape and sidecar allocations here never collect (a full nursery spills
//!   to old space), so handles in hand stay valid across them.
//! - A keyed object keeps its slots across a prototype change: its keys replay
//!   in order on the new root, so every offset is unchanged.
//!
//! # See also
//! - `crate::object::shape_body` — the prototype and dictionary fields.
//! - `crate::object::prototype_change` — the spec checks before a change.

use crate::object::shape_body::{self, ShapePrototype};
use crate::object::{self, JsObject, ObjectPrototype, ShapeHandle};
use crate::*;

impl Interpreter {
    /// Root shape of objects whose `[[Prototype]]` is `prototype`.
    pub(crate) fn instance_root(
        &mut self,
        prototype: &ObjectPrototype,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        let shape_prototype = match *prototype {
            ObjectPrototype::Null => return Ok(shape_body::null_root(&self.gc_heap)),
            ObjectPrototype::Object(mut proto) => {
                if let Some(root) = object::cached_instance_root(proto, &self.gc_heap) {
                    // A heap-only installer may have created it unregistered.
                    self.shape_runtime.register_root(&self.gc_heap, root);
                    return Ok(root);
                }
                let _no_collection = self.gc_heap.always_allocate_scope();
                let root = self.shape_runtime.new_root(
                    &mut self.gc_heap,
                    ShapePrototype::Object(proto),
                    &mut |_: &mut dyn FnMut(*mut RawGc)| {},
                )?;
                object::cache_instance_root(&mut proto, &mut self.gc_heap, root)?;
                return Ok(root);
            }
            ObjectPrototype::Value(value) => ShapePrototype::Value(value),
            ObjectPrototype::Proxy(proxy) => ShapePrototype::Value(Value::proxy(proxy)),
        };
        let _no_collection = self.gc_heap.always_allocate_scope();
        self.shape_runtime.new_root(
            &mut self.gc_heap,
            shape_prototype,
            &mut |_: &mut dyn FnMut(*mut RawGc)| {},
        )
    }

    /// Root shape of `null`-prototype objects.
    #[must_use]
    pub(crate) fn null_prototype_root(&self) -> ShapeHandle {
        shape_body::null_root(&self.gc_heap)
    }

    /// Root shape of objects whose `[[Prototype]]` is the ordinary object
    /// `prototype`, or `null`.
    pub(crate) fn object_root(
        &mut self,
        prototype: Option<JsObject>,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        self.instance_root(&match prototype {
            Some(proto) => ObjectPrototype::Object(proto),
            None => ObjectPrototype::Null,
        })
    }

    /// Root shape of objects whose `[[Prototype]]` is the value `prototype`:
    /// `null`, an ordinary object, or a non-ordinary object value.
    pub(crate) fn value_root(
        &mut self,
        prototype: Value,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        self.instance_root(&object::object_prototype_of_value(Some(prototype)))
    }

    /// §10.1.2.1 OrdinarySetPrototypeOf for the ordinary object `obj`:
    /// `Ok(false)` when the spec rejects the change. The object moves to the
    /// new prototype's lineage — its keys replayed on the new root, or the new
    /// lineage's dictionary shape in dictionary mode.
    ///
    /// # Spec
    ///
    /// - <https://tc39.es/ecma262/#sec-ordinarysetprototypeof>
    pub(crate) fn set_ordinary_prototype(
        &mut self,
        obj: &mut JsObject,
        proto: Option<Value>,
    ) -> Result<bool, VmError> {
        let new_proto = match object::prototype_change(*obj, &self.gc_heap, proto) {
            object::PrototypeChange::Unchanged => return Ok(true),
            object::PrototypeChange::Rejected => return Ok(false),
            object::PrototypeChange::To(prototype) => prototype,
        };
        let _no_collection = self.gc_heap.always_allocate_scope();
        let root = self.instance_root(&new_proto)?;
        let shape = object::shape(*obj, &self.gc_heap);
        let new_shape = if shape_body::is_dictionary_of(shape) {
            shape_body::dictionary_of(root)
        } else {
            let ordered = object::shape_ordered_slot_attrs(&self.gc_heap, shape);
            let mut no_extra = |_: &mut dyn FnMut(*mut RawGc)| {};
            self.replay_slots_from(root, obj, &ordered, &mut no_extra)?
        };
        object::install_prototype_shape(*obj, &mut self.gc_heap, new_shape);
        Ok(true)
    }
}
