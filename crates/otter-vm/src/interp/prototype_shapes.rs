//! Prototype/capacity/state hidden-class roots and prototype changes.
//!
//! # Contents
//! - [`Interpreter::instance_root`] / [`Interpreter::object_root`] — the root
//!   shape of objects created with a given prototype and inline capacity.
//! - [`Interpreter::set_ordinary_prototype`] — §10.1.2.1
//!   OrdinarySetPrototypeOf for ordinary objects.
//!
//! # Invariants
//! - A shape fixes its objects' `[[Prototype]]` (V8 maps, JSC structures,
//!   SpiderMonkey shapes all do), so creating an object picks its prototype's
//!   root and changing a prototype changes the shape; nothing else stores it.
//! - Each ordinary prototype caches a traced chain of capacity/state roots on itself;
//!   each root owns its dictionary companion. `null` uses the heap root chain,
//!   and a non-ordinary prototype gets a fresh root that lives as long as the
//!   objects built from it.
//! - Root preparation uses the full runtime handle scope; old-space shapes
//!   remain collectable, and allocation failure propagates before publication.
//! - Prototype changes preserve inline capacity and every bank-relative field
//!   location and lookup/extensibility/prototype-role/provisional state.
//!   Keys replay in order on a root of the same capacity and semantic state.
//!
//! # See also
//! - `crate::object::shape_body` — the prototype and dictionary fields.
//! - `crate::object::prototype_change` — the spec checks before a change.

use crate::object::shape_body;
use crate::object::{self, JsObject, ObjectPrototype, ShapeHandle, ShapeState};
use crate::rooting::RootScopeExt;
use crate::*;

impl Interpreter {
    /// Root shape of objects whose `[[Prototype]]` is `prototype`.
    pub(crate) fn instance_root(
        &mut self,
        prototype: &ObjectPrototype,
        inline_capacity: usize,
        state: ShapeState,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        // The handle frame registers the full runtime root provider. The heap
        // owner roots its prototype and partial result through real collection.
        self.with_handle_scope(|this, scope| {
            let value = match *prototype {
                ObjectPrototype::Null => Value::null(),
                ObjectPrototype::Object(object) => Value::object(object),
                ObjectPrototype::Proxy(proxy) => Value::proxy(proxy),
                ObjectPrototype::Value(value) => value,
            };
            let prototype = this.scoped_value(scope, value);
            let root = object::heap_instance_root(
                object::object_prototype_of_value(Some(this.escape_scoped(prototype))),
                &mut this.gc_heap,
                inline_capacity,
                state,
                &mut |_| {},
            )?;
            this.shape_runtime.register_shape(&this.gc_heap, root);
            if let Some(proto) = this.escape_scoped(prototype).as_object() {
                this.shape_runtime
                    .register_shape(&this.gc_heap, object::shape(proto, &this.gc_heap));
            }
            Ok(root)
        })
    }

    /// Root shape of `null`-prototype objects.
    #[must_use]
    pub(crate) fn null_prototype_root(&self) -> ShapeHandle {
        shape_body::root_for_layout(
            shape_body::null_root(&self.gc_heap),
            object::DEFAULT_INLINE_CAPACITY,
            ShapeState::ORDINARY,
        )
        .expect("default null root")
    }

    /// Root shape of objects whose `[[Prototype]]` is the ordinary object
    /// `prototype`, or `null`.
    pub(crate) fn object_root(
        &mut self,
        prototype: Option<JsObject>,
        inline_capacity: usize,
        state: ShapeState,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        self.instance_root(
            &match prototype {
                Some(proto) => ObjectPrototype::Object(proto),
                None => ObjectPrototype::Null,
            },
            inline_capacity,
            state,
        )
    }

    /// Root shape of objects whose `[[Prototype]]` is the value `prototype`:
    /// `null`, an ordinary object, or a non-ordinary object value.
    pub(crate) fn value_root(
        &mut self,
        prototype: Value,
        inline_capacity: usize,
        state: ShapeState,
    ) -> Result<ShapeHandle, otter_gc::OutOfMemory> {
        self.instance_root(
            &object::object_prototype_of_value(Some(prototype)),
            inline_capacity,
            state,
        )
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
        let mut proto = proto.unwrap_or(Value::null());
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: the actual receiver slot and this pending prototype value
        // stay stationary until publication. Both are forwarded even when
        // root preparation or replay fails after a cap-triggered collection.
        unsafe {
            roots.add_object(obj);
            roots.add_value(&mut proto);
        }
        match object::prototype_change(*obj, &self.gc_heap, Some(proto)) {
            object::PrototypeChange::Unchanged => return Ok(true),
            object::PrototypeChange::Rejected => return Ok(false),
            object::PrototypeChange::To(_) => {}
        }
        let shape = object::shape(*obj, &self.gc_heap);
        let target = shape_body::state_of(shape).with_dictionary(false);
        let root = self.instance_root(
            &object::object_prototype_of_value(Some(proto)),
            shape_body::inline_capacity_of(shape),
            target,
        )?;
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
