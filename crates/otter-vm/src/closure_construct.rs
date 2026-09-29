//! Out-of-line closure state: the own-property bag, the constructor's
//! prototype slot proof and learned instance size, and a `[[Prototype]]`
//! override.
//!
//! # Contents
//! - [`ClosureRareBody`] — GC body a closure points at once it needs any of
//!   this state (JSC's `FunctionRareData`, V8's `FunctionRareData`).
//! - [`alloc_closure_rare_with_roots`] — allocator.
//! - `prepare_closure_prototype_slot` — the constructor prototype slot proof.
//!
//! # Invariants
//! - A plain closure has no rare record; a closure gets one the first time it
//!   receives an own property or a prototype override, and keeps it.
//! - `own_props` is the sole traced property-bag handle; zero means absent.
//!   It owns an ordinary internal property table, never a JS exotic receiver.
//!   Dictionary migration may retain an empty metadata sidecar; that does not
//!   invalidate a matching shape with no overridden slot attributes.
//! - `prototype_shape` is a traced shape handle: hidden classes are
//!   collectable, and generated code compares the live bag's shape handle
//!   against it, so the record keeps the proof's shape alive (a later shape
//!   can never take its cell). A cached slot is usable only after matching
//!   the live bag shape and ordinary unmodified descriptor guards.
//! - `proto_override` is meaningful only while the owning closure's
//!   [`crate::closure::CLOSURE_LOOKUP_PROTO_OVERRIDE`] bit is set; it is traced
//!   either way.
//! - Generated construct code reads the record through the closure's rare
//!   handle; a null handle is a guard miss.
//!
//! # See also
//! - `constructor_profile` — sampling and pre-collection flushing.
//! - `closure` — the closure body and its rare handle.

use crate::{JsObject, Value};
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::SlotVisitor;
use otter_gc::{GcHeap, OutOfMemory};
use std::cell::Cell;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ClosureRareBody`].
pub const CLOSURE_RARE_BODY_TYPE_TAG: u8 = 0x30;

/// Handle to a closure's out-of-line record.
pub type ClosureRareHandle = otter_gc::Gc<ClosureRareBody>;

/// Out-of-line closure state. `#[repr(C)]`: generated construct code reads
/// the bag, the prototype slot proof and the learned size at fixed offsets.
#[repr(C)]
#[derive(Debug)]
pub struct ClosureRareBody {
    pub(crate) own_props: JsObject,
    pub(crate) prototype_shape: crate::object::ShapeHandle,
    pub(crate) prototype_slot: u32,
    pub(crate) learned_instance_fields: Cell<u16>,
    pub(crate) proto_override: Value,
}

/// Byte offset of the own-property bag handle in the payload.
pub const CLOSURE_RARE_OWN_PROPS_OFFSET: usize = std::mem::offset_of!(ClosureRareBody, own_props);
/// Byte offset of the prototype-slot proof's bag shape in the payload.
pub const CLOSURE_RARE_PROTOTYPE_SHAPE_OFFSET: usize =
    std::mem::offset_of!(ClosureRareBody, prototype_shape);
/// Byte offset of the prototype-slot proof's slot index in the payload.
pub const CLOSURE_RARE_PROTOTYPE_SLOT_OFFSET: usize =
    std::mem::offset_of!(ClosureRareBody, prototype_slot);
/// Byte offset of the learned instance size in the payload.
pub const CLOSURE_RARE_LEARNED_INSTANCE_FIELDS_OFFSET: usize =
    std::mem::offset_of!(ClosureRareBody, learned_instance_fields);

impl Default for ClosureRareBody {
    fn default() -> Self {
        Self {
            own_props: JsObject::null(),
            prototype_shape: crate::object::ShapeHandle::null(),
            prototype_slot: 0,
            learned_instance_fields: Cell::new(0),
            proto_override: Value::undefined(),
        }
    }
}

impl otter_gc::SafeTraceable for ClosureRareBody {
    const TYPE_TAG: u8 = CLOSURE_RARE_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        use crate::pelt::PeltField as _;
        self.own_props.pelt_trace(visitor);
        self.proto_override.pelt_trace(visitor);
        if !self.prototype_shape.is_null() {
            visitor(std::ptr::addr_of_mut!(self.prototype_shape).cast::<otter_gc::raw::RawGc>());
        }
    }
}

impl ClosureRareBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        crate::code_liveness::visit_value(&self.proto_override, visitor);
    }
}

/// Allocate an empty rare record while exposing the caller's roots across
/// any allocation-triggered collection.
///
/// # Errors
/// Surfaces [`OutOfMemory`] verbatim.
pub fn alloc_closure_rare_with_roots(
    heap: &mut GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ClosureRareHandle, OutOfMemory> {
    heap.alloc_with_roots(ClosureRareBody::default(), external_visit)
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
            Some((shape, slot))
        });
        let Some(rare) = closure.rare(&self.gc_heap) else {
            return;
        };
        let (shape, slot) = proof.unwrap_or((crate::object::ShapeHandle::null(), 0));
        self.gc_heap.with_payload(rare, |rare| {
            rare.prototype_shape = shape;
            rare.prototype_slot = slot;
        });
        self.gc_heap.record_write(rare, &shape);
    }
}
