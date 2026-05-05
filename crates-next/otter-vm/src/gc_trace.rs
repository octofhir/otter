//! GC root-tracing scaffolding.
//!
//! Every value-model type that will move from `Rc<RefCell<…>>`
//! to `Gc<…>` over tasks 76–83 implements [`GcTrace`] here.
//! The trait fires from [`crate::runtime_state::RuntimeState::trace_roots`]
//! during a full GC. Bodies are intentionally empty today —
//! Early phase entries started as stubs while the value model was
//! still `Rc`-shared. As each migration task lands, the matching
//! body is filled in so roots expose real `Gc<…>` slots without
//! adding new wiring points.
//!
//! # Contents
//!
//! - [`GcTrace`] — the trait every traceable VM type
//!   implements.
//! - [`GcRootVisitor`] — visitor type alias mirroring the
//!   `otter-gc` slot-pointer contract (`*mut RawGc`).
//!
//! # Invariants
//!
//! - A migrated implementation must visit its own `Gc<…>` slot and
//!   delegate to nested VM containers that own additional slots.
//! - A still-unmigrated implementation may remain an empty stub
//!   only when the type genuinely holds no active GC handles yet.
//! - The visitor receives `*mut RawGc` (slot pointer, not
//!   value) so the scavenger can rewrite the slot when an
//!   object moves — matches the
//!   [`otter_gc::heap::RootSlotVisitor`] contract.
//!
//! # See also
//!
//! - GC architecture plan §4 (root sources + walker).
//! - Task 75 — root enumeration.

use crate::array::JsArray;
use crate::collections::{JsMap, JsSet, JsWeakMap, JsWeakSet};
use crate::error_classes::ErrorClassRegistry;
use crate::generator::JsGenerator;
use crate::microtask::{Microtask, MicrotaskQueue};
use crate::native_function::NativeFunction;
use crate::object::JsObject;
use crate::promise::{JsPromiseHandle, PurePromise};
use crate::regexp::JsRegExp;
use crate::symbol::{JsSymbol, SymbolRegistry, WellKnownSymbols};
use crate::{AsyncFrameState, BoundFunction, Frame, IteratorState};
use otter_gc::RawGc;

/// Visitor passed by the GC root walker. Receives the **slot
/// pointer** (`*mut RawGc`), not the value, so the scavenger
/// can rewrite the slot in place when an object moves.
pub type GcRootVisitor<'a> = dyn FnMut(*mut RawGc) + 'a;

/// Walked by the GC during full collection. Implementations
/// emit one slot pointer per outgoing `Gc<…>` reference held
/// by `self`.
///
/// # Implementation contract
///
/// - Must not allocate against the GC heap.
/// - Must not retain `visitor`.
/// - Must visit every `Gc<…>` slot reachable from `self`,
///   transitively for any `Rc<…>` subgraph that holds GC
///   handles (post-migration).
pub trait GcTrace {
    /// Walk every outgoing GC reference owned by `self`.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>);
}

// ---------------------------------------------------------------------------
// Root adapters.
//
// Each migrated impl yields the slot pointer for its own `Gc<T>`
// handle. Still-parked value shapes keep explicit stubs until the
// matching migration task replaces their storage.
// ---------------------------------------------------------------------------

// Task-76 note: `UpvalueCell` is now `otter_gc::Gc<UpvalueCellBody>`,
// a foreign type — its outgoing references are walked by the GC's
// own [`otter_gc::Traceable`] dispatch on `UpvalueCellBody`, not
// by `GcTrace`. Closure spines that hold `UpvalueCell` slots are
// reached through [`crate::Value::trace_value_slots`] inside
// [`Frame`]'s register walk (still a stub today; lands fully when
// the interpreter starts triggering GC from inside its alloc
// paths).

impl GcTrace for JsObject {
    /// Emit the storage address of `*self` as a slot pointer.
    ///
    /// `JsObject` is `otter_gc::Gc<ObjectBody>` —
    /// `#[repr(transparent)]` over a 4-byte cage offset. The
    /// scavenger may rewrite that offset in place when
    /// `ObjectBody` moves, so we hand it the field's storage
    /// address (cast to `*mut RawGc`). The full ObjectBody
    /// graph (prototype, slot values, symbol-keyed values) is
    /// walked by [`crate::object::ObjectBody`]'s
    /// [`otter_gc::SafeTraceable`] impl during marking — this
    /// trait only has to surface the *root* slot.
    ///
    /// # See also
    ///
    /// - [`crate::object::ObjectBody::trace_slots_safe`].
    /// - GC architecture plan §4.2 (root sources).
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let p = self as *const JsObject as *mut RawGc;
        visitor(p);
    }
}

impl GcTrace for JsArray {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let p = self as *const JsArray as *mut RawGc;
        visitor(p);
    }
}

impl GcTrace for JsMap {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let p = self as *const JsMap as *mut RawGc;
        visitor(p);
    }
}

impl GcTrace for JsSet {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let p = self as *const JsSet as *mut RawGc;
        visitor(p);
    }
}

impl GcTrace for JsWeakMap {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let p = self as *const JsWeakMap as *mut RawGc;
        visitor(p);
    }
}

impl GcTrace for JsWeakSet {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let p = self as *const JsWeakSet as *mut RawGc;
        visitor(p);
    }
}

impl GcTrace for JsPromiseHandle {
    /// Stub — body lands with task 82 (promise / iterator /
    /// generator migration).
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for PurePromise {
    /// Stub — body lands with task 82.
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for IteratorState {
    /// Stub — body lands with task 82.
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for JsGenerator {
    /// Stub — body lands with task 82.
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for BoundFunction {
    /// Stub — body lands with task 83 (bound / native / regexp
    /// migration).
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for NativeFunction {
    /// Stub — body lands with task 83.
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for JsRegExp {
    /// Stub — body lands with task 83.
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for Frame {
    /// Stub — body lands incrementally as locals / register
    /// window / accumulator / `this` migrate (task 76+).
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for AsyncFrameState {
    /// Stub — body lands with task 82 (async / generator
    /// parked-frame migration).
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for Microtask {
    /// Stub — body lands when microtask payloads carry `Gc`
    /// handles (task 82).
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for MicrotaskQueue {
    /// Stub — body lands with task 82, where each queued
    /// [`Microtask`] payload's `Gc` slots get walked.
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for JsSymbol {
    /// Stub — body lands with task 83 if symbols ever carry
    /// inline GC references (`description` becomes `Gc<…>`).
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for WellKnownSymbols {
    /// Stub — well-known symbols are leaf primitives; the
    /// trace ladder only matters once `description` is
    /// `Gc`-stored (task 83).
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for SymbolRegistry {
    /// Stub — registry stores `JsSymbol`s; body lands with
    /// task 83.
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for ErrorClassRegistry {
    /// Trace constructor/prototype objects owned by the registry.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        ErrorClassRegistry::trace_gc_roots(self, visitor);
    }
}
