//! Active interpreter frame-root providers.
//!
//! # Contents
//! - [`FrameRoots`] — safe trait implemented by VM-owned frame-stack tracers.
//! - [`FrameRootProviders`] — LIFO registry of active dispatch-loop stacks.
//!
//! # Invariants
//! - Providers are pushed on dispatch-loop entry and popped before the
//!   provider object goes out of scope.
//! - Root tracing happens during a stop-the-world GC pause.
//! - Raw provider dereference is kept in this crate; VM crates only create raw
//!   provider pointers.
//!
//! # See also
//! - [`crate::heap::GcHeap::push_frame_roots`]

use crate::compressed::RawGc;

/// Safe callback surface for VM-owned active frame stacks.
pub trait FrameRoots {
    /// Visit every mutable raw root slot reachable from this active frame stack.
    fn trace(&self, visitor: &mut dyn FnMut(*mut RawGc));
}

/// LIFO registry of active frame-stack root providers.
#[derive(Default)]
pub struct FrameRootProviders {
    providers: Vec<*const dyn FrameRoots>,
}

impl FrameRootProviders {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push `provider` and return the new stack depth.
    pub fn push(&mut self, provider: *const dyn FrameRoots) -> usize {
        self.providers.push(provider);
        self.providers.len()
    }

    /// Pop entries back down to `depth`.
    pub fn pop_to(&mut self, depth: usize) {
        debug_assert!(depth <= self.providers.len());
        self.providers.truncate(depth);
    }

    /// Visit every registered provider in registration order.
    pub fn trace(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for &provider in &self.providers {
            // SAFETY: providers are pushed only for lexical scopes where the
            // pointed-to `FrameRoots` object remains alive, and GC root tracing
            // runs synchronously before the matching pop.
            unsafe { (&*provider).trace(visitor) };
        }
    }

    /// Number of currently registered providers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// `true` when no providers are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

/// Type-erased raw frame/cold-pool pair whose dereference is owned by the GC.
pub struct RawFrameRoots<S, C> {
    stack: *const S,
    cold_pool: *const C,
    trace: fn(&S, &C, &mut dyn FnMut(*mut RawGc)),
}

impl<S, C> RawFrameRoots<S, C> {
    /// Build a provider from raw VM-owned frame-stack components.
    ///
    /// The returned provider must only be registered while both pointers remain
    /// live and while `trace` is valid for the pointed-to concrete types.
    #[must_use]
    pub fn new(
        stack: *const S,
        cold_pool: *const C,
        trace: fn(&S, &C, &mut dyn FnMut(*mut RawGc)),
    ) -> Self {
        Self {
            stack,
            cold_pool,
            trace,
        }
    }
}

impl<S, C> FrameRoots for RawFrameRoots<S, C> {
    fn trace(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: `RawFrameRoots::new` instances are registered only for scopes
        // where the pointed-to frame stack and cold-frame pool remain live.
        unsafe { (self.trace)(&*self.stack, &*self.cold_pool, visitor) };
    }
}
