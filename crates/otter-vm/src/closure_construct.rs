//! Canonical closure-owned property and constructor observation fields.
//!
//! # Contents
//! - [`ClosureConstructHeader`] exposes fixed-width state to generated allocation.
//! - A guarded slot proof retains no moving prototype value.
//!
//! # Invariants
//! - `own_props` is the sole traced property-bag handle; zero means absent.
//!   It owns an ordinary internal property table, never a JS exotic receiver.
//!   Dictionary migration may retain an empty metadata sidecar; that does not
//!   invalidate a matching shape with no overridden slot attributes.
//! - Shape handles are interned and immortal. A cached slot is usable only after
//!   matching the live bag shape and ordinary unmodified descriptor guards.
//! - `last_instance` is a weak observation, never a root. Nonzero requires an
//!   entry in the existing pending-constructor ledger; GC flush clears it before
//!   movement. Generated allocation may replace it only while that entry exists.
//!
//! # See also
//! - `constructor_profile` — sampling and pre-collection flushing.
//! - `closure` — ownership and tracing of the machine-facing prefix.

use crate::JsObject;
use std::cell::Cell;

#[repr(C)]
#[derive(Debug)]
pub(crate) struct ClosureConstructHeader {
    pub(crate) own_props: JsObject,
    pub(crate) prototype_shape: u32,
    pub(crate) prototype_slot: u32,
    pub(crate) learned_instance_fields: Cell<u16>,
    pub(crate) last_instance: Cell<JsObject>,
}

impl Default for ClosureConstructHeader {
    fn default() -> Self {
        Self {
            own_props: JsObject::null(),
            prototype_shape: 0,
            prototype_slot: 0,
            learned_instance_fields: Cell::new(0),
            last_instance: Cell::new(JsObject::null()),
        }
    }
}

impl ClosureConstructHeader {
    pub(crate) fn observed_receiver(&self) -> Option<JsObject> {
        let receiver = self.last_instance.get();
        (!receiver.is_null()).then_some(receiver)
    }

    pub(crate) fn take_receiver(&self) -> Option<JsObject> {
        let receiver = self.last_instance.replace(JsObject::null());
        (!receiver.is_null()).then_some(receiver)
    }
}

impl crate::Interpreter {
    /// Cache only the immutable shape/slot proof, never the prototype value.
    /// Generated preparation still checks live descriptor and storage state.
    pub(crate) fn prepare_closure_prototype_slot(
        &mut self,
        roots: &crate::call_ops::SyncJsCallRoots,
    ) {
        let Some(closure) = roots.construct_target().as_closure(&self.gc_heap) else {
            return;
        };
        if let Some(mut bag) = closure.own_props(&self.gc_heap)
            && crate::object::shape(bag, &self.gc_heap).is_null()
        {
            // Canonical property bags can start in dictionary storage. Migration
            // roots the bag; the registered call roots preserve both constructor
            // identities and the resolved prototype across shape allocations.
            self.migrate_slow_to_fast(&mut bag);
        }
        let closure = roots.construct_target().as_closure(&self.gc_heap).unwrap();
        let proof = closure.own_props(&self.gc_heap).and_then(|bag| {
            if !matches!(
                crate::object::lookup_own(bag, &self.gc_heap, "prototype"),
                crate::object::PropertyLookup::Data { .. }
            ) {
                return None;
            }
            let shape = crate::object::shape(bag, &self.gc_heap);
            if shape.is_null() {
                return None;
            }
            let slot = crate::object::shape_offset_of_str(&self.gc_heap, shape, "prototype")?;
            Some((shape.offset(), slot))
        });
        self.gc_heap.with_payload(closure.handle, |body| {
            let (shape, slot) = proof.unwrap_or((0, 0));
            body.construct.prototype_shape = shape;
            body.construct.prototype_slot = slot;
        });
    }
}
