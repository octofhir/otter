//! Payload access, cell relayout and mutator write barriers.
//!
//! # Contents
//! - Checked payload borrows and in-place trimming.
//! - Individual and bulk stores through the existing barrier owner.
//!
//! # Invariants
//! - Mutable payload operations barrier the resulting outgoing edges.
//! - Typed and raw store APIs enter the same write-barrier implementation.
//!
//! # See also
//! - [`crate::barrier`].

use super::verification::debug_assert_handle_live;
use super::{GcHeap, read_payload};
use crate::compressed::{Gc, RawGc, cage_base};
use crate::header::GcHeader;
use crate::store::GcStore;
use crate::trace::Traceable;

impl GcHeap {
    /// Read-only access to the payload of a heap-allocated
    /// value. The closure receives `&T`; never holds the
    /// reference past return.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `handle.is_null()`.
    /// Diagnostic: read the header type tag behind a handle without
    /// touching the payload. `None` for null handles.
    pub fn debug_header_tag<T: Traceable>(&self, handle: Gc<T>) -> Option<u8> {
        if handle.is_null() {
            return None;
        }
        // SAFETY: header read only; single mutator.
        unsafe { Some((*handle.as_header_ptr()).type_tag()) }
    }

    /// Immutable access to the payload of a heap-allocated value.
    #[inline]
    pub fn read_payload<T: Traceable, R>(&self, handle: Gc<T>, f: impl FnOnce(&T) -> R) -> R {
        read_payload(handle, f)
    }

    /// Mutable access to the payload of a heap-allocated value.
    ///
    /// **Caller is responsible for [`Self::write_barrier`]** on
    /// every new outgoing GC reference established inside the
    /// closure. The closure runs under the same single-mutator
    /// GC contract as [`Self::alloc`].
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `handle.is_null()`.
    #[inline]
    pub fn with_payload<T: Traceable, R>(
        &mut self,
        handle: Gc<T>,
        f: impl FnOnce(&mut T) -> R,
    ) -> R {
        debug_assert!(!handle.is_null(), "with_payload on null handle");
        debug_assert_handle_live::<T>(handle, "with_payload");
        // SAFETY: same contract as [`Self::read_payload`]; the
        // exclusive `&mut self` upholds payload uniqueness.
        let result = unsafe {
            let payload =
                (handle.as_header_ptr() as *mut u8).add(std::mem::size_of::<GcHeader>()) as *mut T;
            f(&mut *payload)
        };
        // A change made here may store a pointer no barrier saw: a cell the
        // concurrent marker already visited is visited again at the end.
        if self.marking.mutator_phase() {
            // SAFETY: the handle names a live cell.
            unsafe { self.marking.regray_changed(handle.as_header_ptr()) };
        }
        result
    }

    /// [`Self::with_payload`] for a change that rewrites which words of the
    /// body hold pointers, on a type that declared
    /// [`Traceable::LAYOUT_LOCKED`]: while marking runs, an old cell's layout
    /// lock is held around it, so the concurrent marker never reads a body
    /// half rewritten.
    pub fn with_payload_relayout<T: Traceable, R>(
        &mut self,
        handle: Gc<T>,
        f: impl FnOnce(&mut T) -> R,
    ) -> R {
        const { assert!(T::LAYOUT_LOCKED, "relayout is for layout-locked types") };
        let header = handle.as_header_ptr();
        // SAFETY: the handle names a live cell.
        let lock = self.marking.is_marking() && unsafe { !(*header).is_young() };
        if lock {
            // SAFETY: as above; an old cell is never forwarded.
            unsafe { (*header).lock_layout() };
        }
        let result = self.with_payload(handle, f);
        if lock {
            // SAFETY: as above; this thread holds the lock.
            unsafe { (*header).unlock_layout() };
        }
        result
    }

    /// Drop `bytes` from the front of a cell's trailing storage in place: its
    /// header and the `fixed` bytes that start the cell (header included)
    /// move forward by `bytes`, and the vacated range becomes a free filler,
    /// so the trailing words after the dropped ones stay where they are. The
    /// moved header keeps its flags. Returns the cell's new offset; `None`
    /// leaves the cell untouched when it cannot move: on a large-object page,
    /// while remembered (the remembered list names it by offset), or while
    /// marking or sweeping walk cells by address.
    ///
    /// # Safety
    ///
    /// `cell` names a live cell whose only reference the caller rewrites to
    /// the returned offset; `fixed` covers the header and the cell's fixed
    /// part, which its trailing storage follows.
    pub unsafe fn trim_cell_start(
        &mut self,
        cell: RawGc,
        fixed: usize,
        bytes: usize,
    ) -> Option<RawGc> {
        if bytes == 0
            || !bytes.is_multiple_of(crate::page::CELL_SIZE)
            || self.marking.is_marking()
            || self.sweeping.is_some()
        {
            return None;
        }
        // SAFETY: per docstring, `cell` names a live in-cage header.
        unsafe {
            let header = cage_base().add(cell.0 as usize) as *mut GcHeader;
            let space = crate::page::Page::space_of(header as *const u8);
            if matches!(
                space,
                crate::page::SpaceKind::Large | crate::page::SpaceKind::NewLarge
            ) || (*header).is_remembered()
            {
                return None;
            }
            let size = (*header).size_bytes() as usize;
            if fixed + bytes > size {
                return None;
            }
            let moved = header.cast::<u8>().add(bytes);
            std::ptr::copy(header.cast::<u8>(), moved, fixed);
            moved
                .add(crate::header::HEADER_SIZE_BYTES_OFFSET)
                .cast::<u32>()
                .write_unaligned((size - bytes) as u32);
            header.write(GcHeader::new_free(bytes as u32));
            Some(RawGc(cell.0 + bytes as u32))
        }
    }

    /// Single hot-path write barrier for a pointer-field store.
    ///
    /// Caller must:
    /// - have already performed the underlying store of `child`
    ///   into the parent (inline field or malloc-owned side
    ///   storage reachable from the payload),
    /// - pass the parent's `Gc<T>` (so the heap can locate the
    ///   parent's header for card marking).
    pub fn write_barrier<T: ?Sized, U: ?Sized>(&mut self, parent: Gc<T>, child: Gc<U>) {
        if parent.is_null() {
            return;
        }
        let parent_header = parent.as_header_ptr();
        // SAFETY: parent is non-null and live.
        unsafe {
            crate::barrier::write_barrier(
                parent_header,
                child.raw(),
                &mut self.marking,
                &mut self.remembered_parents,
            );
        }
    }

    /// Record the outgoing edges established by storing `value` into
    /// `parent`.
    ///
    /// This is the safe contributor-facing mutation hook. Callers do
    /// not provide raw slot pointers and do not call barriers
    /// manually; they only pass the parent object and the value that
    /// was just stored. Card marking is header-granular, so the slot's
    /// actual location (inline or malloc-owned side storage) is
    /// irrelevant.
    pub fn record_write<T: ?Sized, V: GcStore + ?Sized>(&mut self, parent: Gc<T>, value: &V) {
        if parent.is_null() {
            return;
        }
        let mut record = |edge: crate::GcEdge| {
            self.write_barrier_raw(parent, edge.raw());
        };
        value.visit_gc_edges(&mut record);
    }

    /// Barrier for `values` copied into `parent` in one bulk fill, such as
    /// a grown element slab. Outside marking a young parent owes nothing,
    /// and an old one is remembered at its first young child: the parent,
    /// not the slot, is what a scavenge re-traces. While marking, every
    /// white child is shaded as an individual store would.
    pub fn record_bulk_writes<T: ?Sized, V: GcStore>(
        &mut self,
        parent: Gc<T>,
        values: impl IntoIterator<Item = V>,
    ) {
        if parent.is_null() {
            return;
        }
        let header = parent.as_header_ptr();
        let marking = self.marking.is_marking();
        // SAFETY: a non-null handle names a live object's header.
        let remembered = unsafe { !(*header).is_old() || (*header).is_remembered() };
        if !marking && remembered {
            return;
        }
        for value in values {
            // SAFETY: the parent's header is live for the call; every edge
            // is an in-cage child offset by construction.
            unsafe { self.record_write_at(header, &value) };
            // SAFETY: as above.
            if !marking && unsafe { (*header).is_remembered() } {
                return;
            }
        }
    }

    /// Write barrier for a caller that already holds the parent's header
    /// address rather than a typed [`Gc`].
    ///
    /// Generated code derives the header once for the store itself and hands
    /// it straight to the barrier; re-deriving a `Gc` from it would only throw
    /// the address away and rebuild it.
    ///
    /// # Safety
    /// `parent_header` must name a live object's header for the call.
    pub unsafe fn record_write_at<V: GcStore + ?Sized>(
        &mut self,
        parent_header: *mut GcHeader,
        value: &V,
    ) {
        if parent_header.is_null() {
            return;
        }
        let marking = &mut self.marking;
        let remembered = &mut self.remembered_parents;
        value.visit_gc_edges(&mut |edge: crate::GcEdge| {
            // SAFETY: forwarded from this function's contract; the edge is an
            // in-cage child offset by construction.
            unsafe { crate::barrier::write_barrier(parent_header, edge.raw(), marking, remembered) }
        });
    }

    /// Address of the incremental-marking flag byte.
    ///
    /// The insertion half of [`crate::barrier::write_barrier`] fires only
    /// while a marking cycle is in progress. Compiled code tests that one byte
    /// inline so an ordinary pointer store never leaves generated code.
    #[must_use]
    pub fn marking_flag_addr(&self) -> *const u8 {
        self.marking.marking_flag_addr()
    }

    /// Type-erased write barrier for callers that hold the
    /// child as a [`RawGc`] rather than a typed `Gc<U>`.
    /// Equivalent to [`Self::write_barrier`] otherwise.
    /// Record one parent→child edge from a raw child handle.
    ///
    /// For writes performed outside the typed mutation helpers — e.g. host
    /// payloads installed into an old-space body right after allocation,
    /// where the children were collected through a trace visitor rather
    /// than a `GcStore` value.
    pub fn record_write_edge<T: ?Sized>(&mut self, parent: Gc<T>, child: RawGc) {
        self.write_barrier_raw(parent, child);
    }

    pub(crate) fn write_barrier_raw<T: ?Sized>(&mut self, parent: Gc<T>, child: RawGc) {
        if parent.is_null() {
            return;
        }
        let parent_header = parent.as_header_ptr();
        // SAFETY: parent is non-null and live.
        unsafe {
            crate::barrier::write_barrier(
                parent_header,
                child,
                &mut self.marking,
                &mut self.remembered_parents,
            );
        }
    }
}
