//! Out-of-line closure state: the own-property bag, the function's
//! `prototype` slot, constructor-owned layout families, and a
//! `[[Prototype]]` override.
//!
//! # Contents
//! - [`ClosureRareBody`] — GC body a closure points at once it needs any of
//!   this state (JSC's `FunctionRareData`, V8's `FunctionRareData`).
//! - [`alloc_closure_rare_with_roots`] — allocator.
//!
//! # Invariants
//! - A plain closure has no rare record; a closure gets one the first time it
//!   receives an own property or a prototype override, and keeps it.
//! - `own_props` is the sole traced property-bag handle; zero means absent.
//!   It owns an ordinary internal property table, never a JS exotic receiver.
//!   Dictionary migration may retain an empty metadata sidecar; that does not
//!   invalidate a matching shape with no overridden slot attributes.
//! - `prototype` holds the function's `prototype` property (V8's
//!   `prototype_or_initial_map`): the hole until the default object is
//!   allocated, then its value. It never lives in the bag, so assigning
//!   `F.prototype` gives the closure no own-property bag. The property is
//!   non-enumerable and non-configurable; `prototype_writable` is its only
//!   attribute that can change (true → false). Storing a different value
//!   detaches every constructor family and clears `constructor_layouts`.
//! - `prototype_ordinary` is set exactly while `prototype` holds an ordinary
//!   object: generated `instanceof` reads it in place of the slot's type.
//! - `proto_override` is meaningful only while the owning closure's
//!   [`crate::closure::CLOSURE_LOOKUP_PROTO_OVERRIDE`] bit is set; it is traced
//!   either way.
//! - Generated construct code reads the record through the closure's rare
//!   handle; a null handle is a guard miss.
//!
//! # See also
//! - `crate::constructor_layout` — seven terminal construction samples.
//! - `closure` — the closure body and its rare handle.

use crate::{JsObject, Value};
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::SlotVisitor;
use otter_gc::{GcHeap, OutOfMemory};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ClosureRareBody`].
pub const CLOSURE_RARE_BODY_TYPE_TAG: u8 = 0x30;

/// Handle to a closure's out-of-line record.
pub type ClosureRareHandle = otter_gc::Gc<ClosureRareBody>;

/// Out-of-line closure state. `#[repr(C)]`: generated construct and
/// `instanceof` code reads the bag and the `prototype` slot; construct code reads the layout head
/// at fixed offsets.
#[repr(C)]
#[derive(Debug)]
pub struct ClosureRareBody {
    pub(crate) own_props: JsObject,
    pub(crate) prototype_writable: bool,
    pub(crate) prototype_ordinary: bool,
    pub(crate) constructor_layouts: crate::constructor_layout::ConstructorLayout,
    pub(crate) prototype: Value,
    pub(crate) proto_override: Value,
}

/// Byte offset of the own-property bag handle in the payload.
pub const CLOSURE_RARE_OWN_PROPS_OFFSET: usize = std::mem::offset_of!(ClosureRareBody, own_props);
/// Byte offset of the flag that the `prototype` slot holds an ordinary object.
pub const CLOSURE_RARE_PROTOTYPE_ORDINARY_OFFSET: usize =
    std::mem::offset_of!(ClosureRareBody, prototype_ordinary);
/// Byte offset of the `prototype` slot (hole until allocated) in the payload.
pub const CLOSURE_RARE_PROTOTYPE_OFFSET: usize = std::mem::offset_of!(ClosureRareBody, prototype);
/// Byte offset of the traced constructor family head.
pub(crate) const CLOSURE_RARE_CONSTRUCTOR_LAYOUTS_OFFSET: usize =
    std::mem::offset_of!(ClosureRareBody, constructor_layouts);

impl Default for ClosureRareBody {
    fn default() -> Self {
        Self {
            own_props: JsObject::null(),
            prototype_writable: true,
            prototype_ordinary: false,
            constructor_layouts: crate::constructor_layout::ConstructorLayout::null(),
            prototype: Value::hole(),
            proto_override: Value::undefined(),
        }
    }
}

impl otter_gc::SafeTraceable for ClosureRareBody {
    const TYPE_TAG: u8 = CLOSURE_RARE_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        use crate::pelt::PeltField as _;
        self.own_props.pelt_trace(visitor);
        self.constructor_layouts.pelt_trace(visitor);
        self.prototype.pelt_trace(visitor);
        self.proto_override.pelt_trace(visitor);
    }
}

impl ClosureRareBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        crate::code_liveness::visit_value(&self.prototype, visitor);
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
