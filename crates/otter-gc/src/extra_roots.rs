//! Type-erased runtime root source registered with a heap.
//!
//! # Contents
//!
//! - [`ExtraRootSource`] — root owners, pre-collection observation hooks, and
//!   weak-reference resolution, and the post-mark table sweep.
//! - [`RootSet`] — which roots a collection needs: every root, or only those
//!   that may name a young cell.
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
//! - A scavenge asks for [`RootSet::Young`]. A source may then skip a slot
//!   only when the slot can never hold a young cell: a scavenge neither
//!   moves nor frees old-space cells, so such a slot is never rewritten.
//! - Weak observations never enter the strong root walk. After strong and
//!   ephemeron reachability is complete, the collector resolves them before
//!   recycling from-space or sweeping old cells.
//!
//! # See also
//!
//! - [`crate::heap::GcHeap::register_extra_roots`]

use crate::compressed::RawGc;
use crate::heap::GcHeap;

/// Which roots one collection visits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSet {
    /// Every root: a full collection marks from them and may move any cell.
    All,
    /// A scavenge: roots that may name a young cell. Slots that only ever
    /// hold old-space cells may be skipped.
    Young,
}

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

    /// Drop owner-held references to cells a full collection's marking left
    /// unreached, before the sweep frees them. Mark bits are final: the
    /// ephemeron fixpoint has run. The hook must not allocate, mark, reenter
    /// JavaScript or retain the heap reference; it may only forget entries.
    fn sweep_weak(&self, _heap: &GcHeap) {}

    /// Rewrite or forget non-owning observations after strong and ephemeron
    /// tracing. The resolver returns a survivor's current handle, or `None`
    /// for a dead target. It never marks or evacuates an object.
    ///
    /// The source must not allocate in the GC heap, reenter JavaScript,
    /// change registrations or retain the resolver. It may update its own
    /// scalar observations and invalidate dependent code. Repeated visits
    /// are allowed, including through composite registrations.
    fn process_weak(&self, _resolve: &mut dyn FnMut(RawGc) -> Option<RawGc>) {}

    /// Visit the mutable raw root slots of `roots` owned by this source.
    fn visit_extra_roots(&self, roots: RootSet, visitor: &mut dyn FnMut(*mut RawGc));
}

/// Type-erased root source registration held by [`crate::heap::GcHeap`].
#[derive(Clone, Copy)]
pub struct ExtraRoots {
    data: *const (),
    thunk: unsafe fn(*const (), RootSet, &mut dyn FnMut(*mut RawGc)),
    prepare: unsafe fn(*const (), &GcHeap),
    sweep: unsafe fn(*const (), &GcHeap),
    weak: unsafe fn(*const (), &mut dyn FnMut(RawGc) -> Option<RawGc>),
}

impl ExtraRoots {
    /// Create a registration for `source`.
    #[must_use]
    pub fn new<S: ExtraRootSource>(source: &S) -> Self {
        unsafe fn thunk<S: ExtraRootSource>(
            data: *const (),
            roots: RootSet,
            visitor: &mut dyn FnMut(*mut RawGc),
        ) {
            // SAFETY: `ExtraRoots::new` records the concrete `S` pointer, and
            // the heap registration contract requires the source to outlive the
            // installed `ExtraRoots` value.
            unsafe { (&*(data as *const S)).visit_extra_roots(roots, visitor) };
        }

        unsafe fn prepare<S: ExtraRootSource>(data: *const (), heap: &GcHeap) {
            // SAFETY: the same source lifetime and concrete type as the root thunk.
            unsafe { (&*(data as *const S)).prepare_collection(heap) };
        }

        unsafe fn sweep<S: ExtraRootSource>(data: *const (), heap: &GcHeap) {
            // SAFETY: the same source lifetime and concrete type as the root thunk.
            unsafe { (&*(data as *const S)).sweep_weak(heap) };
        }

        unsafe fn weak<S: ExtraRootSource>(
            data: *const (),
            resolve: &mut dyn FnMut(RawGc) -> Option<RawGc>,
        ) {
            // SAFETY: the same source lifetime and concrete type as the root thunk.
            unsafe { (&*(data as *const S)).process_weak(resolve) };
        }

        Self {
            data: source as *const S as *const (),
            thunk: thunk::<S>,
            prepare: prepare::<S>,
            sweep: sweep::<S>,
            weak: weak::<S>,
        }
    }

    /// Prepare registered owner observations before any collection work.
    /// Composite sources must forward this hook as well as [`Self::visit`].
    pub fn prepare_collection(self, heap: &GcHeap) {
        // SAFETY: the registered source is live throughout this GC pause.
        unsafe { (self.prepare)(self.data, heap) };
    }

    /// Run the source's post-mark weak pass. Composite sources must forward
    /// this hook as well as [`Self::visit`].
    pub fn sweep_weak(self, heap: &GcHeap) {
        // SAFETY: the registered source is live throughout this GC pause.
        unsafe { (self.sweep)(self.data, heap) };
    }

    /// Resolve non-owning observations. Composite sources must forward this
    /// hook as well as [`Self::visit`].
    pub fn process_weak(self, resolve: &mut dyn FnMut(RawGc) -> Option<RawGc>) {
        // SAFETY: the registered source is live throughout this GC pause.
        unsafe { (self.weak)(self.data, resolve) };
    }

    /// Visit the source's roots. Public so a composite
    /// [`ExtraRootSource`] (e.g. a native-call scope that adds its own
    /// argument roots on top of the interpreter's runtime roots) can
    /// re-dispatch into an inner registration without the VM crate
    /// needing raw-pointer dereference of its own.
    pub fn visit(self, roots: RootSet, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: callers install `ExtraRoots` only for scopes where `data`
        // still points at the original `ExtraRootSource`.
        unsafe { (self.thunk)(self.data, roots, visitor) };
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
        fn visit_extra_roots(&self, _roots: RootSet, _visitor: &mut dyn FnMut(*mut RawGc)) {}
    }

    #[test]
    fn weak_owner_resolves_survivors_before_recycling_and_drops_dead_targets() {
        use crate::test_support::OpaqueLeaf;
        use std::cell::Cell;

        struct Observations {
            live: Cell<RawGc>,
            dead: Cell<RawGc>,
        }
        impl ExtraRootSource for Observations {
            fn visit_extra_roots(&self, _: RootSet, _: &mut dyn FnMut(*mut RawGc)) {}
            fn process_weak(&self, resolve: &mut dyn FnMut(RawGc) -> Option<RawGc>) {
                for slot in [&self.live, &self.dead] {
                    slot.set(resolve(slot.get()).unwrap_or(RawGc::NULL));
                }
            }
        }
        struct Composite(ExtraRoots);
        impl ExtraRootSource for Composite {
            fn visit_extra_roots(&self, _: RootSet, _: &mut dyn FnMut(*mut RawGc)) {}
            fn process_weak(&self, resolve: &mut dyn FnMut(RawGc) -> Option<RawGc>) {
                self.0.process_weak(resolve);
            }
        }

        let mut heap = GcHeap::new().expect("heap");
        let mut live = heap.alloc(OpaqueLeaf { payload: 11 }).expect("live").raw();
        let original = live;
        let observations = Observations {
            live: Cell::new(live),
            dead: Cell::new(heap.alloc(OpaqueLeaf { payload: 22 }).expect("dead").raw()),
        };
        let composite = Composite(ExtraRoots::new(&observations));
        let _registration = heap.register_extra_roots(ExtraRoots::new(&composite));
        let _duplicate = heap.register_extra_roots(ExtraRoots::new(&composite));
        for _ in 0..2 {
            heap.collect_minor_with_roots(&mut |visitor| visitor(&mut live))
                .expect("scavenge");
            assert_eq!(observations.live.get(), live);
            assert_eq!(observations.dead.get(), RawGc::NULL);
        }
        assert_ne!(live, original);
        heap.collect_full(&mut |visitor| visitor(&mut live))
            .expect("live full collection");
        assert_eq!(observations.live.get(), live);
        heap.start_incremental_mark_phase(&mut |_| {})
            .expect("split mark");
        heap.finish_incremental_mark_phase(&mut |_| {})
            .expect("finish split mark");
        heap.sweep_phase();
        assert_eq!(observations.live.get(), RawGc::NULL);
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
            fn visit_extra_roots(&self, _roots: RootSet, _visitor: &mut dyn FnMut(*mut RawGc)) {
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
            fn visit_extra_roots(&self, roots: RootSet, visitor: &mut dyn FnMut(*mut RawGc)) {
                self.0.visit(roots, visitor);
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
        heap.finish_incremental_mark_phase(&mut external).unwrap();
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
            fn visit_extra_roots(&self, _roots: RootSet, _visitor: &mut dyn FnMut(*mut RawGc)) {
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
