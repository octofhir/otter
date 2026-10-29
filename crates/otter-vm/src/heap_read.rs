//! Reading the GC heap from the mutator or a compiler thread.
//!
//! # Contents
//! - [`HeapRead`] — what reading object, shape and context bodies needs: a
//!   payload read and the shape tables. The mutator's [`otter_gc::GcHeap`]
//!   and a compiler thread's [`HeapView`] both answer it.
//! - [`HeapView`] — a compiler thread's read-only view of one isolate's heap.
//!
//! # Invariants
//! - A payload is addressed by the cage alone; reading one needs no heap
//!   state, so a view reads exactly what the heap would.
//! - A compiler thread reads through a view only while it holds the
//!   isolate's heap-read right, which excludes collection: no body it reads
//!   moves or is freed. Bodies the mutator writes meanwhile may read stale;
//!   every fact baked from them is re-verified before its code is published.
//! - Shape tables are shared ([`crate::mutator_owned::MutatorCell`]); a view
//!   reads them only while the mutator is not writing them, and reads
//!   nothing when it is.
//!
//! # See also
//! - `interp::jit_bake_env` — the view's holder.

use crate::mutator_owned::MutatorCell;
use crate::object::shape_interner::WeakShapeTables;

/// What reading heap bodies needs.
pub(crate) trait HeapRead {
    /// Read the payload of the live cell `handle`.
    fn read_payload<T: otter_gc::Traceable, R>(
        &self,
        handle: otter_gc::Gc<T>,
        f: impl FnOnce(&T) -> R,
    ) -> R;

    /// Run `f` over the isolate's shape tables; `None` when they cannot be
    /// read now.
    fn with_shape_tables<R>(&self, f: impl FnOnce(&WeakShapeTables) -> R) -> Option<R>;

    /// The external-reference index of `addr`; `None` when never interned or
    /// unreadable now.
    fn external_ref(&self, addr: usize) -> Option<u32>;

    /// The allocation-site records, whose registered entries never move.
    fn allocation_sites(&self) -> &otter_gc::pretenuring::SiteDirectory;
}

impl HeapRead for otter_gc::GcHeap {
    #[inline]
    fn read_payload<T: otter_gc::Traceable, R>(
        &self,
        handle: otter_gc::Gc<T>,
        f: impl FnOnce(&T) -> R,
    ) -> R {
        otter_gc::GcHeap::read_payload(self, handle, f)
    }

    #[inline]
    fn with_shape_tables<R>(&self, f: impl FnOnce(&WeakShapeTables) -> R) -> Option<R> {
        crate::object::shape_interner::ShapeInterner::with_tables(self, f)
    }

    #[inline]
    fn external_ref(&self, addr: usize) -> Option<u32> {
        self.external_refs().lookup(addr)
    }

    fn allocation_sites(&self) -> &otter_gc::pretenuring::SiteDirectory {
        otter_gc::GcHeap::allocation_sites(self).directory_ref()
    }
}

/// A compiler thread's read-only view of one isolate's heap.
#[derive(Clone)]
pub(crate) struct HeapView {
    /// A read met the mutator changing a shared table: what the bake
    /// read is incomplete, and it is retried.
    contended: std::cell::Cell<bool>,
    shape_tables: std::sync::Arc<MutatorCell<WeakShapeTables>>,
    external_refs: std::sync::Arc<otter_gc::external_refs::SharedExternalRefs>,
    allocation_sites: std::sync::Arc<otter_gc::pretenuring::SiteDirectory>,
}

impl HeapView {
    /// Whether a read since the view was made met the mutator writing.
    pub(crate) fn contended(&self) -> bool {
        self.contended.get()
    }

    /// The view of `heap`, for one of its compilations.
    pub(crate) fn of(heap: &otter_gc::GcHeap) -> Self {
        Self {
            contended: std::cell::Cell::new(false),
            shape_tables: crate::object::shape_interner::ShapeInterner::shared_tables(heap),
            external_refs: heap.shared_external_refs(),
            allocation_sites: heap.allocation_sites().directory(),
        }
    }
}

impl HeapRead for HeapView {
    #[inline]
    fn read_payload<T: otter_gc::Traceable, R>(
        &self,
        handle: otter_gc::Gc<T>,
        f: impl FnOnce(&T) -> R,
    ) -> R {
        otter_gc::read_payload(handle, f)
    }

    fn with_shape_tables<R>(&self, f: impl FnOnce(&WeakShapeTables) -> R) -> Option<R> {
        let Some(tables) = self.shape_tables.read_briefly() else {
            self.contended.set(true);
            return None;
        };
        Some(f(&tables))
    }

    fn external_ref(&self, addr: usize) -> Option<u32> {
        crate::mutator_owned::retry_briefly(|| self.external_refs.try_lookup(addr).ok())
            .unwrap_or_else(|| {
                self.contended.set(true);
                None
            })
    }

    fn allocation_sites(&self) -> &otter_gc::pretenuring::SiteDirectory {
        &self.allocation_sites
    }
}
