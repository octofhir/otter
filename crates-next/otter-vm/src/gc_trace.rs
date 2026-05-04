//! GC root-tracing scaffolding.
//!
//! Every value-model type that will move from `Rc<RefCell<…>>`
//! to `Gc<…>` over tasks 76–83 implements [`GcTrace`] here.
//! The trait fires from [`crate::runtime_state::RuntimeState::trace_roots`]
//! during a full GC. Bodies are intentionally empty today —
//! Phase 1's value model is still `Rc`-shared, so there is
//! nothing to enumerate. The signatures land **before** any
//! migration so that each per-type task in tasks 76+ adds a
//! body, not a fresh trait impl plus a fresh wiring point.
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
//! - All implementations in this file are empty stubs. Any
//!   non-empty body landing here without a corresponding
//!   migration task is a bug.
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
// Phase-1 stubs.
//
// Each impl below stays empty until the matching migration task
// (76+) replaces the type's `Rc<RefCell<…>>` storage with a
// real `Gc<T>` slot. At that point the body learns to yield
// the slot pointer; the wiring in `RuntimeState::trace_roots`
// already calls into it.
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
    /// Stub — body lands with task 77 (JsObject migration).
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for JsArray {
    /// Stub — body lands with task 78 (JsArray migration).
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for JsMap {
    /// Stub — body lands with task 79 (JsMap / JsSet migration).
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for JsSet {
    /// Stub — body lands with task 79.
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for JsWeakMap {
    /// Stub — body lands with task 80 (WeakMap / WeakSet
    /// ephemerons).
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for JsWeakSet {
    /// Stub — body lands with task 80.
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
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
    /// Stub — error-class prototypes are `JsObject`s; body
    /// lands with task 77.
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}
