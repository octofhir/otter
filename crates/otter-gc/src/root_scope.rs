//! Scoped GC rooting: an RAII guard that keeps a set of stack-local
//! handles forwarded across every collection inside a lexical scope.
//!
//! This is the intended replacement for the ad-hoc rooting patterns that
//! have accumulated around allocation call sites (`*_with_roots` closure
//! twins, hand-rolled [`crate::FrameRoots`] providers, re-fetch-from-slot
//! dances): declare the live locals once at scope entry, then allocate
//! freely — a moving collection rewrites the locals in place.
//!
//! ```ignore
//! let mut scope = RootScope::new(&mut heap);
//! // SAFETY: `obj` and `val` outlive `scope` (declared before it).
//! unsafe {
//!     scope.add_raw_slot(&mut obj as *mut Gc<Body> as *mut RawGc);
//!     scope.add_erased(&mut val as *mut _ as *mut (), trace_value_erased);
//! }
//! // ... allocations; `obj` / `val` stay current ...
//! drop(scope); // provider popped
//! ```
//!
//! Higher-level crates wrap the two `unsafe` entry points in typed
//! helpers/macros (otter-vm's `Value` tracer cannot live here — the value
//! representation is a VM concern).
//!
//! # Contents
//! - [`RootScope`] — RAII guard registered as a frame-root scope marker.
//! - [`ErasedSlotTracer`] — type-erased per-slot tracer callback.
//!
//! # Invariants
//! - Every registered slot pointer must outlive the scope (the scope is
//!   popped in `Drop`, tracing happens synchronously during GC pauses).
//! - Scopes nest LIFO by construction (Rust drop order); `Drop` truncates
//!   the provider stack back to the scope's entry depth, so a leaked or
//!   out-of-order drop can only over-pop its own descendants.
//! - Slots live in the heap's registry-owned slot stack, tagged with the
//!   scope's marker depth. Opening a scope and adding slots allocate nothing
//!   once that stack has grown; an outer scope never adds slots while an
//!   inner scope is open.
//!
//! # See also
//! - [`crate::frame_roots`] — the provider registry and slot stack this
//!   builds on.

use crate::compressed::RawGc;
use crate::heap::GcHeap;

/// Type-erased tracer for one rooted slot: forwards every `RawGc` the
/// slot transitively holds *in place*.
///
/// # Safety
/// Implementations cast the erased pointer back to the concrete slot
/// type; callers must register the matching pointer/tracer pair.
pub type ErasedSlotTracer = unsafe fn(*mut (), &mut dyn FnMut(*mut RawGc));

/// Tracer for a slot that is itself a bare GC handle (`Gc<T>` /
/// `RawGc`): the slot pointer *is* the root slot.
///
/// # Safety
/// `slot` must point at a live `RawGc`-representable handle.
pub unsafe fn trace_raw_handle_slot(slot: *mut (), visitor: &mut dyn FnMut(*mut RawGc)) {
    visitor(slot.cast::<RawGc>());
}

/// RAII rooting scope. See the module docs for usage.
pub struct RootScope {
    heap: *mut GcHeap,
    depth: usize,
}

impl RootScope {
    /// Open a scope on `heap`. The guard registers itself as a
    /// frame-root marker and unregisters on drop.
    pub fn new(heap: &mut GcHeap) -> Self {
        let depth = heap.push_root_scope();
        Self { heap, depth }
    }

    /// Root a slot that is a bare GC handle (`Gc<T>`, `RawGc`, or any
    /// `#[repr(transparent)]` wrapper around one).
    ///
    /// # Safety
    /// `slot` must point at such a handle and outlive this scope.
    pub unsafe fn add_raw_slot(&mut self, slot: *mut RawGc) {
        // SAFETY: forwarded from this function's contract.
        unsafe { self.add_erased(slot.cast::<()>(), trace_raw_handle_slot) };
    }

    /// Root an arbitrary slot with a matching type-erased tracer.
    ///
    /// # Safety
    /// `slot` must outlive this scope and `tracer` must interpret it at
    /// its concrete type.
    pub unsafe fn add_erased(&mut self, slot: *mut (), tracer: ErasedSlotTracer) {
        // SAFETY: the heap owns the slot stack and outlives every scope
        // opened on it; no collection runs while the slot is pushed.
        unsafe { (*self.heap).push_root_scope_slot(self.depth, slot, tracer) };
    }
}

impl Drop for RootScope {
    fn drop(&mut self) {
        // SAFETY: the heap owns the provider registry and outlives every
        // scope opened on it; scopes drop LIFO, and truncation to the
        // entry depth also cleans up any leaked descendant scopes.
        unsafe { (*self.heap).pop_frame_roots_to(self.depth) };
    }
}
