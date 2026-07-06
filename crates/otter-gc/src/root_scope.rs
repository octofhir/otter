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
//! - [`RootScope`] — RAII guard registered as a frame-root provider.
//! - [`ErasedSlotTracer`] — type-erased per-slot tracer callback.
//!
//! # Invariants
//! - Every registered slot pointer must outlive the scope (the scope is
//!   popped in `Drop`, tracing happens synchronously during GC pauses).
//! - Scopes nest LIFO by construction (Rust drop order); `Drop` truncates
//!   the provider stack back to the scope's entry depth, so a leaked or
//!   out-of-order drop can only over-pop its own descendants.
//! - The slot list lives in a `Box` so the provider pointer registered
//!   with the heap stays stable even if the guard value moves.
//!
//! # See also
//! - [`crate::frame_roots`] — the provider registry this builds on.

use crate::compressed::RawGc;
use crate::frame_roots::FrameRoots;
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

struct RootScopeSlots {
    entries: Vec<(*mut (), ErasedSlotTracer)>,
}

impl FrameRoots for RootScopeSlots {
    fn trace(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for &(slot, tracer) in &self.entries {
            // SAFETY: `RootScope`'s contract — every registered slot
            // outlives the scope, and the tracer matches the slot type.
            unsafe { tracer(slot, visitor) };
        }
    }
}

/// RAII rooting scope. See the module docs for usage.
pub struct RootScope {
    heap: *mut GcHeap,
    depth: usize,
    slots: Box<RootScopeSlots>,
}

impl RootScope {
    /// Open a scope on `heap`. The guard registers itself as a
    /// frame-root provider and unregisters on drop.
    pub fn new(heap: &mut GcHeap) -> Self {
        let slots = Box::new(RootScopeSlots {
            entries: Vec::new(),
        });
        let provider: *const dyn FrameRoots = &*slots;
        let depth = heap.push_frame_roots(provider) - 1;
        Self { heap, depth, slots }
    }

    /// Root a slot that is a bare GC handle (`Gc<T>`, `RawGc`, or any
    /// `#[repr(transparent)]` wrapper around one).
    ///
    /// # Safety
    /// `slot` must point at such a handle and outlive this scope.
    pub unsafe fn add_raw_slot(&mut self, slot: *mut RawGc) {
        self.slots
            .entries
            .push((slot.cast::<()>(), trace_raw_handle_slot));
    }

    /// Root an arbitrary slot with a matching type-erased tracer.
    ///
    /// # Safety
    /// `slot` must outlive this scope and `tracer` must interpret it at
    /// its concrete type.
    pub unsafe fn add_erased(&mut self, slot: *mut (), tracer: ErasedSlotTracer) {
        self.slots.entries.push((slot, tracer));
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
