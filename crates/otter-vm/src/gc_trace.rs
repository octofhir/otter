//! GC root-tracing scaffolding.
//!
//! Every GC-managed value-model type implements [`GcTrace`] here.
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
use crate::dynamic_import::DynamicImportRegistry;
use crate::error_classes::ErrorClassRegistry;
use crate::generator::JsGenerator;
use crate::microtask::{Microtask, MicrotaskQueue};
use crate::native_function::NativeFunction;
use crate::object::JsObject;
use crate::promise::{JsPromiseHandle, PurePromise};
use crate::regexp::JsRegExp;
use crate::symbol::{JsSymbol, SymbolRegistry, WellKnownSymbols};
use crate::timers::TimerCallbacks;
use crate::weak_refs::{JsFinalizationRegistry, JsWeakRef};
use crate::{AsyncFrameState, BoundFunction, Frame, IteratorState};
use otter_gc::raw::RawGc;

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

impl GcTrace for JsWeakRef {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let p = self as *const JsWeakRef as *mut RawGc;
        visitor(p);
    }
}

impl GcTrace for JsFinalizationRegistry {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let p = self as *const JsFinalizationRegistry as *mut RawGc;
        visitor(p);
    }
}

impl GcTrace for JsPromiseHandle {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let p = self as *const JsPromiseHandle as *mut RawGc;
        visitor(p);
    }
}

impl GcTrace for PurePromise {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let p = self as *const PurePromise as *mut RawGc;
        visitor(p);
    }
}

impl GcTrace for IteratorState {
    /// Iterator bodies are traced by `SafeTraceable`; roots carry
    /// `IteratorHandle` slots through `Value::Iterator`.
    fn trace_gc_roots(&self, _visitor: &mut GcRootVisitor<'_>) {}
}

impl GcTrace for JsGenerator {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        let p = self as *const JsGenerator as *mut RawGc;
        visitor(p);
    }
}

impl GcTrace for BoundFunction {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.trace_value_slots(visitor);
    }
}

impl GcTrace for NativeFunction {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.trace_value_slots(visitor);
    }
}

impl GcTrace for JsRegExp {
    /// Emit the storage address of `*self` as a slot pointer.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.trace_value_slots(visitor);
    }
}

impl GcTrace for Frame {
    /// Trace frame locals, registers, receiver, and parked state.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.trace_frame_slots(visitor);
    }
}

impl GcTrace for AsyncFrameState {
    /// Trace async result promise.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.result_promise.trace_gc_roots(visitor);
    }
}

impl GcTrace for Microtask {
    /// Trace queued callback, receiver, arguments, and promise
    /// capability slots. Finalization callbacks use this path so
    /// held values remain live until the cleanup job runs.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.trace_gc_slots(visitor);
    }
}

impl GcTrace for MicrotaskQueue {
    /// Trace every queued microtask payload.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.trace_gc_slots(visitor);
    }
}

impl GcTrace for TimerCallbacks {
    /// Trace every registered timer-callback payload so the JS
    /// callback + extra arguments survive any GC that occurs
    /// between scheduling and firing.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.trace_gc_slots(visitor);
    }
}

impl GcTrace for DynamicImportRegistry {
    /// Trace every pending dynamic-import promise so it survives
    /// any GC between scheduling and settlement.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.trace_gc_slots(visitor);
    }
}

impl GcTrace for JsSymbol {
    /// Emit the storage address of the embedded `SymbolHandle` so the
    /// scavenger can rewrite the compressed offset if the body moves.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.trace_value_slots(visitor);
    }
}

impl GcTrace for WellKnownSymbols {
    /// Visit each well-known singleton so its body stays live.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        for sym in self.entries() {
            sym.trace_gc_roots(visitor);
        }
    }
}

impl GcTrace for SymbolRegistry {
    /// Visit every registered symbol so `Symbol.for(k)` survivors stay
    /// live across collections.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        self.for_each_entry(|sym| sym.trace_gc_roots(visitor));
    }
}

impl GcTrace for ErrorClassRegistry {
    /// Trace constructor/prototype objects owned by the registry.
    fn trace_gc_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        ErrorClassRegistry::trace_gc_roots(self, visitor);
    }
}
