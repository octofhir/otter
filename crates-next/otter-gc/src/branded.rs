//! Experimental isolate-branded GC session API.
//!
//! Task 93 adds a compile-time brand over the existing page-based
//! heap. The brand does not replace the collector backend; it makes
//! the owning isolate part of the Rust type shape for persistent
//! roots, weak handles, and mutator sessions.
//!
//! # Contents
//!
//! - [`with_gc_session`] — enters a heap with a fresh isolate brand.
//! - [`GcSession`] / [`MutationSession`] — short-lived mutator access.
//! - [`Root`] — isolate-branded persistent root over the collector's
//!   internal global-handle table.
//! - [`Weak`] — isolate-branded weak handle shape.
//!
//! # Invariants
//!
//! - The `'iso` brand is invariant and created only by
//!   [`with_gc_session`].
//! - [`Root::get`] and [`Weak::upgrade`] require a matching
//!   [`GcSession<'iso, '_>`], so cross-isolate use is rejected by
//!   the type checker.
//! - Heap access remains explicit through [`GcSession`]; this module
//!   does not reintroduce thread-local heap lookup.
//!
//! # See also
//!
//! - [Task 93](../../../docs/new-engine/tasks/93-gc-branded-session-api.md)
//! - [GC architecture plan](../../../docs/new-engine/gc-architecture.md)

use std::marker::PhantomData;

use crate::compressed::{Gc, RawGc};
use crate::handle::GlobalHandle;
use crate::heap::GcHeap;
use crate::oom::OutOfMemory;
use crate::trace::Traceable;

type InvariantBrand<'iso> = PhantomData<fn(&'iso ()) -> &'iso ()>;

/// Enter `heap` with a fresh isolate brand.
///
/// The brand lifetime is introduced by the higher-ranked closure and
/// cannot be named by the caller. Values branded inside one call
/// therefore cannot be dereferenced by a session from another call.
pub fn with_gc_session<R>(
    heap: &mut GcHeap,
    f: impl for<'iso> FnOnce(GcSession<'iso, '_>) -> R,
) -> R {
    f(GcSession {
        heap,
        _iso: PhantomData,
    })
}

/// Short-lived branded mutator session for one isolate.
///
/// The first lifetime brands the isolate/heap. The second lifetime is
/// the active mutator turn and prevents the session from outliving the
/// heap borrow.
pub struct GcSession<'iso, 'gc> {
    heap: &'gc mut GcHeap,
    _iso: InvariantBrand<'iso>,
}

/// Alias used by native/VM APIs that want mutator terminology.
pub type MutationSession<'iso, 'gc> = GcSession<'iso, 'gc>;

impl<'iso> GcSession<'iso, '_> {
    /// Borrow the underlying heap explicitly.
    #[must_use]
    pub fn heap(&self) -> &GcHeap {
        self.heap
    }

    /// Mutably borrow the underlying heap explicitly.
    #[must_use]
    pub fn heap_mut(&mut self) -> &mut GcHeap {
        self.heap
    }

    /// Allocate into the young generation through this session.
    ///
    /// # Errors
    /// Returns [`OutOfMemory`] when the heap cap or cage refuses the
    /// allocation.
    pub fn alloc<T: Traceable>(&mut self, value: T) -> Result<Gc<T>, OutOfMemory> {
        self.heap.alloc(value)
    }

    /// Allocate directly into old generation through this session.
    ///
    /// # Errors
    /// Returns [`OutOfMemory`] when the heap cap or cage refuses the
    /// allocation.
    pub fn alloc_old<T: Traceable>(&mut self, value: T) -> Result<Gc<T>, OutOfMemory> {
        self.heap.alloc_old(value)
    }

    /// Persistently root `gc` under this isolate brand.
    #[must_use]
    pub fn root<T: ?Sized>(&mut self, gc: Gc<T>) -> Root<'iso, T> {
        Root {
            inner: self.heap.global_handles().create(gc),
            _iso: PhantomData,
        }
    }

    /// Build an isolate-branded weak handle shape.
    ///
    /// This is the task-93 type-level API. It intentionally carries
    /// only raw weak metadata and is upgraded only through a matching
    /// session; VM weak/finalization semantics still live in the
    /// existing weak registries.
    #[must_use]
    pub fn weak<T: ?Sized>(&self, gc: Gc<T>) -> Weak<'iso, T> {
        Weak {
            raw: gc.raw(),
            _iso: PhantomData,
            _not_send: PhantomData,
        }
    }
}

/// Isolate-branded persistent root.
///
/// `Root` wraps the existing moving-GC-compatible global handle and
/// adds the `'iso` brand required to read it back.
pub struct Root<'iso, T: ?Sized> {
    inner: GlobalHandle<T>,
    _iso: InvariantBrand<'iso>,
}

impl<'iso, T: ?Sized> Root<'iso, T> {
    /// Read the rooted handle through a matching isolate session.
    #[must_use]
    pub fn get(&self, _session: &GcSession<'iso, '_>) -> Gc<T> {
        self.inner.get()
    }
}

impl<T: ?Sized> std::fmt::Debug for Root<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Root").finish_non_exhaustive()
    }
}

/// Isolate-branded weak handle shape.
///
/// `Weak` is not a strong root. Upgrading requires a matching
/// [`GcSession`], which is the type-level property task 93 needs
/// before the VM migrates concrete weak APIs onto this wrapper.
#[derive(Clone, Copy)]
pub struct Weak<'iso, T: ?Sized> {
    raw: RawGc,
    _iso: InvariantBrand<'iso>,
    _not_send: PhantomData<*const T>,
}

impl<'iso, T: ?Sized> Weak<'iso, T> {
    /// Upgrade through a matching isolate session.
    ///
    /// The current raw weak shape can only reject null handles; the VM
    /// weak/finalization registries continue to decide liveness after a
    /// GC cycle. This method fixes the branded API shape so later weak
    /// migration cannot accidentally expose a context-free upgrade.
    #[must_use]
    pub fn upgrade(&self, _session: &GcSession<'iso, '_>) -> Option<Gc<T>> {
        if self.raw.is_null() {
            None
        } else {
            // SAFETY: `raw` came from `Gc<T>::raw` in
            // `GcSession::weak`.
            Some(unsafe { self.raw.cast() })
        }
    }

    /// Raw weak metadata for diagnostics/tests.
    #[must_use]
    pub const fn raw(self) -> RawGc {
        self.raw
    }
}

impl<T: ?Sized> std::fmt::Debug for Weak<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Weak")
            .field("raw", &self.raw)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use crate::compressed::CAGE_TEST_LOCK;
    use crate::test_support::OpaqueLeaf;

    use super::*;

    #[test]
    fn branded_session_roots_and_reads_persistent_handle() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut heap = GcHeap::new().unwrap();

        with_gc_session(&mut heap, |mut session| {
            let gc = session.alloc(OpaqueLeaf { payload: 93 }).unwrap();
            let root = session.root(gc);

            assert_eq!(root.get(&session).offset(), gc.offset());
        });
    }

    #[test]
    fn branded_weak_upgrade_requires_session_shape() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut heap = GcHeap::new().unwrap();

        with_gc_session(&mut heap, |mut session| {
            let gc = session.alloc(OpaqueLeaf { payload: 94 }).unwrap();
            let weak = session.weak(gc);

            assert_eq!(weak.raw(), gc.raw());
            assert_eq!(weak.upgrade(&session).unwrap().offset(), gc.offset());
        });
    }

    #[test]
    fn branded_shapes_stay_isolate_local() {
        static_assertions::assert_not_impl_any!(GcSession<'static, 'static>: Send, Sync);
        static_assertions::assert_not_impl_any!(Root<'static, OpaqueLeaf>: Send, Sync);
        static_assertions::assert_not_impl_any!(Weak<'static, OpaqueLeaf>: Send, Sync);
    }
}
