//! Type-erased runtime root source registered with a heap.
//!
//! # Contents
//!
//! - [`ExtraRootSource`] — safe trait implemented by runtime owners of roots.
//! - [`ExtraRoots`] — raw-pointer trampoline stored by [`crate::heap::GcHeap`].
//! - [`ExtraRootsGuard`] — RAII registration removed on return or unwind.
//!
//! # Invariants
//!
//! - The source passed to [`ExtraRoots::new`] must outlive its heap registration.
//!   Callers enforce this by pushing/popping the registration around the VM
//!   turn or explicit GC scope.
//! - The heap keeps registrations on a LIFO stack and traces **every** live
//!   entry, so a nested registration never hides an outer scope's roots from
//!   a collection triggered inside the inner scope.
//! - The VM crate implements only the safe trait; the raw pointer dereference is
//!   kept inside this crate's audited unsafe boundary.
//!
//! # See also
//!
//! - [`crate::heap::GcHeap::register_extra_roots`]

use crate::compressed::RawGc;
use crate::heap::GcHeap;

/// Safe callback surface for owner-managed root slots not stored in the heap's
/// handle stack or global handle table.
pub trait ExtraRootSource {
    /// Visit every mutable raw root slot owned by this source.
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc));
}

/// Type-erased root source registration held by [`crate::heap::GcHeap`].
#[derive(Clone, Copy)]
pub struct ExtraRoots {
    data: *const (),
    thunk: unsafe fn(*const (), &mut dyn FnMut(*mut RawGc)),
}

impl ExtraRoots {
    /// Create a registration for `source`.
    #[must_use]
    pub fn new<S: ExtraRootSource>(source: &S) -> Self {
        unsafe fn thunk<S: ExtraRootSource>(data: *const (), visitor: &mut dyn FnMut(*mut RawGc)) {
            // SAFETY: `ExtraRoots::new` records the concrete `S` pointer, and
            // the heap registration contract requires the source to outlive the
            // installed `ExtraRoots` value.
            unsafe { (&*(data as *const S)).visit_extra_roots(visitor) };
        }

        Self {
            data: source as *const S as *const (),
            thunk: thunk::<S>,
        }
    }

    /// Visit the source's roots. Public so a composite
    /// [`ExtraRootSource`] (e.g. a native-call scope that adds its own
    /// argument roots on top of the interpreter's runtime roots) can
    /// re-dispatch into an inner registration without the VM crate
    /// needing raw-pointer dereference of its own.
    pub fn visit(self, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: callers install `ExtraRoots` only for scopes where `data`
        // still points at the original `ExtraRootSource`.
        unsafe { (self.thunk)(self.data, visitor) };
    }

    /// `true` when both registrations dispatch to the same source
    /// object through the same thunk. Used by the heap's root walk to
    /// skip duplicate stack entries (re-entrant scopes registering the
    /// same interpreter) — a missed match only costs an idempotent
    /// re-visit, never a missed root.
    #[must_use]
    pub fn same_source(&self, other: &Self) -> bool {
        std::ptr::eq(self.data, other.data) && std::ptr::fn_addr_eq(self.thunk, other.thunk)
    }
}

/// RAII registration for an owner-managed runtime root source.
///
/// The guard intentionally stores a raw heap pointer so the mutator can keep
/// using `&mut GcHeap` while the registration is active. Dropping it truncates
/// the registration stack to its entry depth, which also cleans up any leaked
/// nested registrations during unwinding.
#[must_use = "dropping the guard immediately unregisters the root source"]
pub struct ExtraRootsGuard {
    heap: *mut GcHeap,
    depth: usize,
}

impl ExtraRootsGuard {
    pub(crate) fn new(heap: &mut GcHeap, depth: usize) -> Self {
        Self { heap, depth }
    }
}

impl Drop for ExtraRootsGuard {
    fn drop(&mut self) {
        // SAFETY: `GcHeap::register_extra_roots` creates the guard from a live
        // heap and the guard cannot outlive the owning VM turn. The source
        // lifetime remains the caller's existing `ExtraRoots` contract.
        unsafe { (*self.heap).pop_extra_roots_to(self.depth) };
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    struct EmptySource;

    impl ExtraRootSource for EmptySource {
        fn visit_extra_roots(&self, _visitor: &mut dyn FnMut(*mut RawGc)) {}
    }

    #[test]
    fn guard_unregisters_source_on_unwind() {
        let mut heap = GcHeap::new().expect("heap");
        let source = EmptySource;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = heap.register_extra_roots(ExtraRoots::new(&source));
            assert!(heap.has_extra_roots());
            panic!("exercise unwind cleanup");
        }));
        assert!(result.is_err());
        assert!(!heap.has_extra_roots());
    }
}
