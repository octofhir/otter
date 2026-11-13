//! Explicit old-space and variable-body allocation.
//!
//! # Contents
//! - Rooted old placement and initialized trailing storage.
//! - Noncollecting and diagnostic allocation with exact cap accounting.
//!
//! # Invariants
//! - Pending bodies are rooted before collecting cap admission.
//! - Every published old body has its young and marking edges barriered.
//!
//! # See also
//! - [`super::accounting`].

use super::{GcHeap, RootSlotVisitor};
use crate::compressed::{Gc, RawGc, cage_base};
use crate::header::GcHeader;
use crate::oom::OutOfMemory;
use crate::page::{CELL_SIZE, LARGE_OBJECT_THRESHOLD, align_up};
use crate::trace::Traceable;
use std::sync::atomic::Ordering;

impl GcHeap {
    /// Allocate a `T` directly in old-space, bypassing
    /// young-gen entirely.
    ///
    /// Explicit non-moving placement for long-lived bodies and backing stores.
    /// Non-moving placement does not keep a body alive: callers still root
    /// handles across collecting operations, and pending payloads are traced
    /// before any cap-triggered collection.
    ///
    /// # Errors
    ///
    /// - [`OutOfMemory::HeapCapExceeded`] — cap was exceeded
    ///   and emergency full GC could not reclaim enough.
    /// - [`OutOfMemory::CageExhausted`] — cage cannot satisfy
    ///   a fresh page request.
    #[inline]
    pub fn alloc_old<T: Traceable>(&mut self, value: T) -> Result<Gc<T>, OutOfMemory> {
        let mut empty = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.alloc_old_with_roots_inner(value, true, 0, &mut empty, |_| {})
    }

    /// Allocate a `T` directly in old-space while keeping caller-supplied
    /// roots and the pending payload live across any cap-triggered full GC.
    ///
    /// This mirrors [`Self::alloc_with_roots`] for non-moving old-space
    /// allocations used by VM handles that are copied into Rust locals.
    ///
    /// # Errors
    ///
    /// Same as [`Self::alloc_old`].
    #[inline]
    pub fn alloc_old_with_roots<T: Traceable>(
        &mut self,
        value: T,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Gc<T>, OutOfMemory> {
        self.alloc_old_trailing_with_roots(value, 0, external_visit)
    }

    /// [`Self::alloc_old_with_roots`] for a body followed by `extra_bytes`
    /// of zeroed trailing storage.
    ///
    /// # Errors
    ///
    /// Same as [`Self::alloc_old`].
    #[inline]
    pub fn alloc_old_trailing_with_roots<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Gc<T>, OutOfMemory> {
        self.alloc_old_with_roots_inner(value, true, extra_bytes, external_visit, |_| {})
    }

    /// Allocate a `T` followed by `extra_bytes` of zeroed trailing storage
    /// directly in old space without running any collection, then let
    /// `initialize` fill it in its final cell.
    ///
    /// For mutations that must not move the caller's unrooted handles. The
    /// cell is booked against the cap without being refused by it; the next
    /// collecting allocation enforces the cap and the old-generation limit.
    ///
    /// # Errors
    ///
    /// [`OutOfMemory::CageExhausted`] if no in-cage page can satisfy the
    /// allocation.
    pub fn alloc_old_trailing_no_collect<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
        initialize: impl FnOnce(&mut T),
    ) -> Result<Gc<T>, OutOfMemory> {
        let mut empty = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let out =
            self.alloc_old_with_roots_inner(value, false, extra_bytes, &mut empty, initialize)?;
        if self.max_heap_bytes != 0 {
            let aligned = align_up(
                std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>() + extra_bytes,
                CELL_SIZE,
            );
            self.tracked_bytes = self.tracked_bytes.saturating_add(aligned as u64);
        }
        Ok(out)
    }

    /// Allocate a diagnostic object directly in old-space without
    /// refusing on [`OutOfMemory::HeapCapExceeded`].
    ///
    /// This is a narrow escape hatch for error objects that make a
    /// just-triggered heap cap catchable by script code. Cage
    /// exhaustion still fails normally, and the allocation is
    /// reflected back into [`Self::tracked_bytes`] so subsequent
    /// mutator allocations continue to observe the exceeded cap.
    ///
    /// # Errors
    ///
    /// [`OutOfMemory::CageExhausted`] if no in-cage page can satisfy
    /// the allocation.
    #[inline]
    pub fn alloc_old_diagnostic<T: Traceable>(&mut self, value: T) -> Result<Gc<T>, OutOfMemory> {
        self.alloc_old_diagnostic_trailing(value, 0)
    }

    /// [`Self::alloc_old_diagnostic`] for a body followed by `extra_bytes`
    /// of zeroed trailing storage.
    ///
    /// # Errors
    ///
    /// As [`Self::alloc_old_diagnostic`].
    #[inline]
    pub fn alloc_old_diagnostic_trailing<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
    ) -> Result<Gc<T>, OutOfMemory> {
        let mut empty = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let out = self.alloc_old_with_roots_inner(value, false, extra_bytes, &mut empty, |_| {})?;
        if self.max_heap_bytes != 0 {
            self.drain_shared_external_releases();
            let aligned = align_up(
                std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>() + extra_bytes,
                CELL_SIZE,
            );
            // This allocator bypassed charged admission, not LAB ownership.
            // Book only its completed physical cell: replacing the ledger with
            // live page bytes would omit the still-charged unused LAB tail and
            // let effective tracking refund that tail a second time.
            self.tracked_bytes = self.tracked_bytes.saturating_add(aligned as u64);
            self.oom_flag.store(true, Ordering::Relaxed);
        }
        Ok(out)
    }

    #[inline]
    pub(super) fn alloc_old_with_roots_inner<T: Traceable>(
        &mut self,
        mut value: T,
        enforce_cap: bool,
        extra_bytes: usize,
        external_visit: &mut RootSlotVisitor<'_>,
        initialize: impl FnOnce(&mut T),
    ) -> Result<Gc<T>, OutOfMemory> {
        // See `alloc_with_roots`: payloads are at most
        // `OBJECT_ALIGNMENT`-aligned, so over-aligned bodies must box.
        const {
            assert!(
                std::mem::align_of::<T>() <= crate::OBJECT_ALIGNMENT,
                "GC body alignment exceeds OBJECT_ALIGNMENT; box the over-aligned field",
            )
        };
        if self.trace_table.get(T::TYPE_TAG).is_none() {
            self.trace_table.register::<T>();
        }
        let total = std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>() + extra_bytes;
        let aligned = align_up(total, CELL_SIZE);
        debug_assert!(
            aligned <= u32::MAX as usize,
            "object size exceeds u32 limit"
        );
        let pending_value = std::ptr::addr_of_mut!(value);
        let mut allocation_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            // SAFETY: `pending_value` points at the stack-owned payload that
            // will be copied into old-space after any cap-triggered full GC.
            // The pending form is traced, not the in-heap one: `extra_bytes`
            // of trailing storage do not exist yet, and a body that walks a
            // trailing array through a stored count would run off the payload
            // and into this frame's stack.
            unsafe {
                T::trace_pending_slots(pending_value, visitor);
            }
        };
        if enforce_cap && self.max_heap_bytes != 0 {
            self.account_or_collect_with_roots(aligned as u64, &mut allocation_roots)?;
        }
        let is_marking = self.marking.is_marking();
        let offset = if aligned > LARGE_OBJECT_THRESHOLD {
            self.large_space.alloc(aligned)?
        } else {
            self.help_old_sweep();
            self.old_space.alloc(aligned)?
        };
        // SAFETY: offset is a fresh in-cage allocation.
        let header_ptr = unsafe { cage_base().add(offset as usize) as *mut GcHeader };
        let payload_ptr =
            unsafe { cage_base().add(offset as usize + std::mem::size_of::<GcHeader>()) as *mut T };
        // SAFETY: freshly-alloc-ed cage region inside a page we
        // own. The header and payload are fully initialised below
        // before any read.
        unsafe {
            std::ptr::write(header_ptr, GcHeader::new(T::TYPE_TAG, aligned as u32));
            std::ptr::write(payload_ptr, value);
            self.marking.record_old_birth(header_ptr);
            if extra_bytes != 0 {
                // Trailing storage starts zeroed so a body can treat it as
                // an array of empty slots without writing every one.
                let tail = (payload_ptr as *mut u8).add(std::mem::size_of::<T>());
                std::ptr::write_bytes(
                    tail,
                    0,
                    aligned - std::mem::size_of::<GcHeader>() - std::mem::size_of::<T>(),
                );
            }
            // The cell is still unreachable. Populate any trailing storage in
            // its final address before the scan below records every outgoing
            // edge established by the initializer.
            initialize(&mut *payload_ptr);
            crate::cell_map::debug_check_map_word(payload_ptr);
            // The payload was installed with `ptr::write`, bypassing the
            // mutator write barrier. Any young children it carries are
            // old→young edges the next scavenge can only discover through
            // the card table, so barrier every edge now. The traced slots
            // themselves may live in malloc-owned side storage (boxed
            // frames, collection buffers); the barrier marks the card of
            // `header_ptr`, never of the slot.
            let marking = &mut self.marking;
            let remembered = &mut self.remembered_parents;
            T::trace_slots(payload_ptr, &mut |slot| {
                // SAFETY (inherited from the enclosing block): `slot`
                // points into the just-written payload's reachable
                // storage; `header_ptr` is the freshly initialised
                // parent header.
                crate::barrier::write_barrier(header_ptr, *slot, marking, remembered);
            });
        }
        if is_marking {
            // The concurrent marker may reach the cell once a store publishes
            // it: its initialized header must be visible first.
            std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        }
        self.count_runtime_alloc(T::TYPE_TAG, aligned);
        // SAFETY: offset references a fresh `T` payload.
        Ok(unsafe { Gc::from_offset(offset) })
    }

    /// Allocate a body followed by `extra_bytes` of zeroed trailing
    /// storage, in old space.
    ///
    /// This is how a body carries a variable-length array without owning
    /// memory outside the heap: the elements live in the same cell, so
    /// the object is self-contained and a page image can carry it. `T`
    /// stores the element count; its trace impl reads the trailing array
    /// through that count.
    ///
    /// Old space, not the nursery, because a backing store lives as long
    /// as the object that owns it and a semispace copy of it would be
    /// pure overhead.
    ///
    /// # Errors
    ///
    /// Same as [`Self::alloc_old_with_roots`].
    pub fn alloc_variable_with_roots<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Gc<T>, OutOfMemory> {
        self.alloc_old_with_roots_inner(value, true, extra_bytes, external_visit, |_| {})
    }

    /// Allocate a variable old-space body and initialize its trailing storage
    /// before the cell is published to the collector.
    ///
    /// `initialize` runs after the fixed body and zeroed tail reach their final
    /// address, but before the allocator scans the new cell for old-to-young
    /// and incremental-marking edges. Values destined for the tail do not exist
    /// in the stack-resident pending body; callers must yield their buffer from
    /// `external_visit` so a pre-allocation collection can rewrite it.
    ///
    /// # Errors
    ///
    /// Same as [`Self::alloc_variable_with_roots`].
    pub fn alloc_variable_with_roots_initialized<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
        external_visit: &mut RootSlotVisitor<'_>,
        initialize: impl FnOnce(&mut T),
    ) -> Result<Gc<T>, OutOfMemory> {
        self.alloc_old_with_roots_inner(value, true, extra_bytes, external_visit, initialize)
    }
}
