//! Type-erased runtime root source registered with a heap.
//!
//! # Contents
//!
//! - [`ExtraRootSource`] — root owners and pre-collection observation hooks.
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
    /// Discard or summarize ephemeral observations before the collector moves
    /// or sweeps any object. This hook is distinct from root enumeration,
    /// which may run after some objects have already moved.
    ///
    /// The hook must not allocate in the GC heap, reenter JavaScript, mutate
    /// registrations or retain the heap reference. Read-only heap access and
    /// owner-managed scalar/cache updates are permitted. Repeated calls are
    /// allowed, including at incremental-mark steps and sweep boundaries.
    fn prepare_collection(&self, _heap: &GcHeap) {}

    /// Visit every mutable raw root slot owned by this source.
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc));
}

/// Type-erased root source registration held by [`crate::heap::GcHeap`].
#[derive(Clone, Copy)]
pub struct ExtraRoots {
    data: *const (),
    thunk: unsafe fn(*const (), &mut dyn FnMut(*mut RawGc)),
    prepare: unsafe fn(*const (), &GcHeap),
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

        unsafe fn prepare<S: ExtraRootSource>(data: *const (), heap: &GcHeap) {
            // SAFETY: the same source lifetime and concrete type as the root thunk.
            unsafe { (&*(data as *const S)).prepare_collection(heap) };
        }

        Self {
            data: source as *const S as *const (),
            thunk: thunk::<S>,
            prepare: prepare::<S>,
        }
    }

    /// Prepare registered owner observations before any collection work.
    /// Composite sources must forward this hook as well as [`Self::visit`].
    pub fn prepare_collection(self, heap: &GcHeap) {
        // SAFETY: the registered source is live throughout this GC pause.
        unsafe { (self.prepare)(self.data, heap) };
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
    fn observation_preparation_precedes_minor_mark_and_sweep_work() {
        use crate::{Gc, test_support::OpaqueLeaf};
        use std::cell::Cell;

        struct Observations {
            pending: Cell<Option<Gc<OpaqueLeaf>>>,
            observed: Cell<u64>,
            preparations: Cell<usize>,
        }
        impl ExtraRootSource for Observations {
            fn prepare_collection(&self, heap: &GcHeap) {
                self.preparations.set(self.preparations.get() + 1);
                if let Some(handle) = self.pending.take() {
                    self.observed
                        .set(heap.read_payload(handle, |body| body.payload));
                }
            }
            fn visit_extra_roots(&self, _visitor: &mut dyn FnMut(*mut RawGc)) {
                assert!(
                    self.pending.get().is_none(),
                    "observations must not enter tracing"
                );
            }
        }
        let mut heap = GcHeap::new().expect("heap");
        let source = Observations {
            pending: Cell::new(Some(
                heap.alloc(OpaqueLeaf { payload: 11 }).expect("sample"),
            )),
            observed: Cell::new(0),
            preparations: Cell::new(0),
        };
        struct Composite(ExtraRoots);
        impl ExtraRootSource for Composite {
            fn prepare_collection(&self, heap: &GcHeap) {
                self.0.prepare_collection(heap);
            }
            fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
                self.0.visit(visitor);
            }
        }
        let composite = Composite(ExtraRoots::new(&source));
        let _first = heap.register_extra_roots(ExtraRoots::new(&composite));
        let _duplicate = heap.register_extra_roots(ExtraRoots::new(&composite));
        let mut external = |_visitor: &mut dyn FnMut(*mut RawGc)| {
            assert!(
                source.pending.get().is_none(),
                "prepare before external roots can move"
            );
        };
        heap.collect_minor_with_roots(&mut external)
            .expect("minor collection");
        assert_eq!(source.observed.get(), 11);
        assert_eq!(source.preparations.get(), 1, "one preparation per owner");
        source.pending.set(Some(
            heap.alloc(OpaqueLeaf { payload: 22 }).expect("sample"),
        ));
        heap.start_incremental_mark_phase(&mut external)
            .expect("mark start");
        assert_eq!(source.observed.get(), 22);
        source.pending.set(Some(
            heap.alloc(OpaqueLeaf { payload: 30 }).expect("sample"),
        ));
        let _ = heap.incremental_mark_step(1);
        assert_eq!(source.observed.get(), 30);
        source.pending.set(Some(
            heap.alloc(OpaqueLeaf { payload: 33 }).expect("sample"),
        ));
        heap.finish_incremental_mark_phase(&mut external);
        assert_eq!(source.observed.get(), 33);
        source.pending.set(Some(
            heap.alloc_old_diagnostic(OpaqueLeaf { payload: 44 })
                .expect("sample"),
        ));
        heap.sweep_phase();
        assert_eq!(source.observed.get(), 44);
        assert!(source.pending.get().is_none());
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

    #[test]
    fn exact_source_query_distinguishes_unrelated_providers() {
        struct OtherSource(u8);

        impl ExtraRootSource for OtherSource {
            fn visit_extra_roots(&self, _visitor: &mut dyn FnMut(*mut RawGc)) {
                let _ = self.0;
            }
        }

        let mut heap = GcHeap::new().expect("heap");
        let registered = OtherSource(1);
        let unrelated = OtherSource(2);
        let registered_roots = ExtraRoots::new(&registered);
        let unrelated_roots = ExtraRoots::new(&unrelated);

        assert!(!heap.has_extra_root_source(registered_roots));
        assert!(!heap.has_extra_root_source(unrelated_roots));
        let guard = heap.register_extra_roots(registered_roots);
        assert!(heap.has_extra_root_source(registered_roots));
        assert!(!heap.has_extra_root_source(unrelated_roots));
        drop(guard);
        assert!(!heap.has_extra_root_source(registered_roots));
    }
}
