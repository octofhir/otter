//! Ordinary rooted allocation and initialized young-cell publication.
//!
//! # Contents
//! - Fast LAB bumps and noncollecting allocation attempts.
//! - Pending payload roots, stress ticks and cold placement policy.
//!
//! # Invariants
//! - Pending payloads and caller roots survive every collecting admission.
//! - Initialization and barriers precede publication; alignment checks remain.
//!
//! # See also
//! - [`super::machine`].

use super::{GcHeap, LabRefill, RootSlotVisitor, TrailingInit};
use crate::collection_goal::FullGcGoal;
use crate::compressed::{Gc, RawGc, cage_base};
use crate::header::GcHeader;
use crate::observation::{GcPauseKind, GcPauseTrigger};
use crate::oom::OutOfMemory;
use crate::page::{CELL_SIZE, LARGE_OBJECT_THRESHOLD, align_up};
use crate::trace::Traceable;

impl GcHeap {
    /// Allocate a `T` on the GC heap.
    ///
    /// The header gets `T::TYPE_TAG`; the value is moved into the
    /// payload area. When marking is active the new object starts
    /// black (black-allocation — a freshly published object is not
    /// traced this cycle).
    ///
    /// # Errors
    ///
    /// - [`OutOfMemory::HeapCapExceeded`] — the configured per-heap
    ///   cap was exceeded and an emergency full GC could not free
    ///   enough room. The slot is **not** materialised.
    /// - [`OutOfMemory::CageExhausted`] — the cage cannot satisfy
    ///   a fresh page request.
    #[inline]
    pub fn alloc<T: Traceable>(&mut self, value: T) -> Result<Gc<T>, OutOfMemory> {
        let mut empty = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.alloc_with_roots(value, &mut empty)
    }

    /// Write a young cell carved from the linear allocation buffer at
    /// `address` and return its handle.
    #[inline(always)]
    pub(super) fn publish_lab_cell<T: Traceable>(
        &mut self,
        address: usize,
        aligned: usize,
        value: T,
        extra_bytes: usize,
        tail: TrailingInit,
        initialize: impl FnOnce(&mut T),
    ) -> Gc<T> {
        // The cage base is 4 GiB-aligned: the low half of the address is
        // the cage offset.
        let offset = address as u32;
        debug_assert_eq!(address - cage_base() as usize, offset as usize);
        // SAFETY: the buffer handed out `aligned` bytes inside a live
        // from-space page; header and payload are fully written before the
        // handle is published, with no safepoint in between.
        unsafe {
            let header_ptr = cage_base().add(offset as usize).cast::<GcHeader>();
            let payload_ptr = header_ptr
                .cast::<u8>()
                .add(std::mem::size_of::<GcHeader>())
                .cast::<T>();
            if extra_bytes != 0 && tail == TrailingInit::Filled {
                // The cell's last word holds any alignment padding; the
                // initializer writes every trailing byte before it.
                header_ptr
                    .cast::<u8>()
                    .add(aligned - CELL_SIZE)
                    .cast::<u64>()
                    .write_unaligned(0);
            }
            std::ptr::write(header_ptr, GcHeader::new_young(T::TYPE_TAG, aligned as u32));
            std::ptr::write(payload_ptr, value);
            if extra_bytes != 0 && tail == TrailingInit::Zeroed {
                let tail = payload_ptr.cast::<u8>().add(std::mem::size_of::<T>());
                std::ptr::write_bytes(
                    tail,
                    0,
                    aligned - std::mem::size_of::<GcHeader>() - std::mem::size_of::<T>(),
                );
            }
            initialize(&mut *payload_ptr);
            crate::cell_map::debug_check_map_word(payload_ptr);
        }
        if self.trace_table.needs_cleanup(T::TYPE_TAG) {
            self.new_space.track_cleanup(offset);
        }
        self.count_runtime_alloc(T::TYPE_TAG, aligned);
        // SAFETY: offset is the cage offset of a freshly written `T` payload.
        unsafe { Gc::from_offset(offset) }
    }

    /// Try to allocate a `T` in young space without running any collection.
    ///
    /// This is the compiled-code fast-path primitive: it succeeds only when the
    /// nursery already has room and cap accounting can be booked immediately.
    /// Any condition that would require a safepoint, emergency full GC, minor
    /// scavenge, old-space overflow, or large-object handling returns `None` so
    /// the caller can enter the normal rooted allocation path.
    #[inline]
    pub fn try_alloc_no_collect<T: Traceable>(&mut self, value: T) -> Option<Gc<T>> {
        self.try_alloc_no_collect_or_return(value).ok()
    }

    /// Try to allocate a `T` in young space without running any collection,
    /// returning the original payload when a safepoint-free allocation is not
    /// possible.
    ///
    /// Compound payload callers can use this to build an owned body once, try
    /// the compiled-code fast boundary, and then hand the same body to
    /// [`Self::alloc_with_roots`] if any guard requires the normal rooted path.
    #[inline]
    pub fn try_alloc_no_collect_or_return<T: Traceable>(&mut self, value: T) -> Result<Gc<T>, T> {
        self.try_alloc_trailing_no_collect_or_return(value, 0, |_| {})
    }

    /// [`Self::try_alloc_no_collect_or_return`] for a body followed by
    /// `extra_bytes` of zeroed trailing storage, which `initialize` fills in
    /// the final cell before the handle is returned.
    #[inline]
    pub fn try_alloc_trailing_no_collect_or_return<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
        initialize: impl FnOnce(&mut T),
    ) -> Result<Gc<T>, T> {
        self.try_alloc_trailing_no_collect_as(value, extra_bytes, TrailingInit::Zeroed, initialize)
    }

    /// [`Self::try_alloc_trailing_no_collect_or_return`] whose `initialize`
    /// writes every one of the `extra_bytes`: the trailing storage is not
    /// zeroed first, only the cell's alignment padding after it.
    #[inline]
    pub fn try_alloc_filled_trailing_no_collect_or_return<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
        initialize: impl FnOnce(&mut T),
    ) -> Result<Gc<T>, T> {
        self.try_alloc_trailing_no_collect_as(value, extra_bytes, TrailingInit::Filled, initialize)
    }

    pub(super) fn try_alloc_trailing_no_collect_as<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
        tail: TrailingInit,
        initialize: impl FnOnce(&mut T),
    ) -> Result<Gc<T>, T> {
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
        if aligned > LARGE_OBJECT_THRESHOLD {
            return Err(value);
        }
        let address = match self.lab.bump(aligned) {
            Some(address) => address,
            None => {
                // The refill declines under tenuring, stress, marking and a
                // full cap; a due major collection keeps the rooted path.
                if self.major_gc_due() || self.refill_lab(aligned) != LabRefill::Ready {
                    return Err(value);
                }
                let Some(address) = self.lab.bump(aligned) else {
                    unreachable!("a refilled buffer holds the cell it was sized for");
                };
                address
            }
        };
        self.runtime_lab_bytes += aligned as u64;
        Ok(self.publish_lab_cell(address, aligned, value, extra_bytes, tail, initialize))
    }

    /// Allocate a `T` while exposing caller-owned root slots to any
    /// emergency full GC or minor scavenge triggered by the allocation.
    ///
    /// Use this for mutator allocation sites whose live roots are not stored
    /// in the heap's handle/global tables, such as VM interpreter frames held
    /// on the Rust call stack. The visitor may be called during this
    /// allocation and must yield rewriteable `RawGc` slots.
    ///
    /// If collection is triggered before the payload is materialised in a heap
    /// cell, the pending `value` is traced as part of the temporary root set.
    /// This keeps GC-bearing fields inside arrays, objects, or other compound
    /// payloads valid across allocation-triggered collection.
    ///
    /// # Errors
    ///
    /// Same as [`Self::alloc`].
    #[inline]
    pub fn alloc_with_roots<T: Traceable>(
        &mut self,
        value: T,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Gc<T>, OutOfMemory> {
        self.alloc_trailing_with_roots(value, 0, external_visit)
    }

    /// Allocate `T` followed by `extra_bytes` of zeroed trailing storage,
    /// under the ordinary generational policy.
    ///
    /// This is how a body carries a variable-length array without owning
    /// memory outside the heap. `T` stores the element count; its trace impl
    /// reads the trailing array through that count.
    ///
    /// Unlike [`Self::alloc_variable_with_roots`] the result is young when
    /// the nursery has room, so a body that dies young — a temporary string,
    /// say — is reclaimed by a scavenge instead of waiting for a major GC.
    ///
    /// # Errors
    ///
    /// Same as [`Self::alloc`].
    #[inline]
    pub fn alloc_trailing_with_roots<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Gc<T>, OutOfMemory> {
        self.alloc_trailing_with_roots_inner(value, extra_bytes, external_visit, |_| {})
    }

    /// [`Self::alloc_trailing_with_roots`] that initializes the trailing
    /// storage before the cell can be observed by any collection.
    ///
    /// `initialize` runs once the fixed body and zeroed tail sit at their
    /// final address and before any barrier scan of the new cell: in the
    /// nursery it runs right after the payload write; when the body lands in
    /// old space (bootstrap tenuring, a large body, or nursery overflow) it
    /// runs before the allocator records the cell's outgoing edges, exactly
    /// as in [`Self::alloc_variable_with_roots_initialized`]. No safepoint
    /// separates the allocation from the initializer, so a body whose trace
    /// reads the tail never exposes uninitialized slots. Values the
    /// initializer copies in must be reachable from `external_visit` (or be
    /// immediates), so a collection this allocation triggers rewrites them
    /// before the copy.
    ///
    /// # Errors
    ///
    /// Same as [`Self::alloc`].
    #[inline]
    pub fn alloc_trailing_with_roots_initialized<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
        external_visit: &mut RootSlotVisitor<'_>,
        initialize: impl FnOnce(&mut T),
    ) -> Result<Gc<T>, OutOfMemory> {
        self.alloc_trailing_with_roots_inner(value, extra_bytes, external_visit, initialize)
    }

    #[inline]
    pub(super) fn alloc_trailing_with_roots_inner<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
        external_visit: &mut RootSlotVisitor<'_>,
        initialize: impl FnOnce(&mut T),
    ) -> Result<Gc<T>, OutOfMemory> {
        // A cell payload sits one `GcHeader` past an
        // `OBJECT_ALIGNMENT`-aligned cell start, so it is at most
        // `OBJECT_ALIGNMENT`-aligned. A body needing more (e.g. an
        // inline `i128`) would be read through a misaligned reference —
        // UB. Box the over-aligned field instead (see `TemporalBody`).
        const {
            assert!(
                std::mem::align_of::<T>() <= crate::OBJECT_ALIGNMENT,
                "GC body alignment exceeds OBJECT_ALIGNMENT; box the over-aligned field",
            )
        };
        // Lazy register so callers never forget. Idempotent.
        if self.trace_table.get(T::TYPE_TAG).is_none() {
            self.trace_table.register::<T>();
        }
        let total = std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>() + extra_bytes;
        let aligned = align_up(total, CELL_SIZE);
        debug_assert!(
            aligned <= u32::MAX as usize,
            "object size exceeds u32 limit"
        );
        // The buffer is non-empty only when a plain young cell is legal, so
        // the bump is the whole policy test; heap cap, stress, marking,
        // tenuring and major-GC triggers run when the slow path refills it.
        if aligned <= LARGE_OBJECT_THRESHOLD
            && let Some(address) = self.lab.bump(aligned)
        {
            self.runtime_lab_bytes += aligned as u64;
            return Ok(self.publish_lab_cell(
                address,
                aligned,
                value,
                extra_bytes,
                TrailingInit::Zeroed,
                initialize,
            ));
        }
        self.alloc_trailing_slow(value, aligned, extra_bytes, external_visit, initialize)
    }

    /// Allocation path behind an empty or exhausted buffer: bootstrap
    /// tenuring, stress and major collections, buffer refill, and the
    /// per-object placements (marking, heap-cap limit, nursery overflow,
    /// large bodies).
    #[inline(never)]
    pub(super) fn alloc_trailing_slow<T: Traceable>(
        &mut self,
        mut value: T,
        aligned: usize,
        extra_bytes: usize,
        external_visit: &mut RootSlotVisitor<'_>,
        initialize: impl FnOnce(&mut T),
    ) -> Result<Gc<T>, OutOfMemory> {
        if self.tenure_all {
            return self.alloc_old_with_roots_inner(
                value,
                true,
                extra_bytes,
                external_visit,
                initialize,
            );
        }
        let pending_value = std::ptr::addr_of_mut!(value);
        let mut allocation_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            // SAFETY: `pending_value` points at the still-unpublished
            // allocation payload on this stack frame. During the STW
            // collection triggered by this allocation, tracing may rewrite any
            // GC slots embedded in that payload before it is copied into the
            // freshly carved heap cell below.
            unsafe {
                T::trace_pending_slots(pending_value, visitor);
            }
        };

        // GC-stress ("gc-zeal"): force a collection so any unrooted
        // `Value`/`Gc<T>` held by the caller across this allocation reads
        // a moved object and fails deterministically. Off unless
        // `OTTER_GC_STRESS` is set. `allocation_roots` keeps the pending
        // payload + caller's roots safe; everything else is meant to move.
        if self.stress_collection_armed() {
            self.gc_stress_counter = self.gc_stress_counter.saturating_add(1);
            if self.gc_stress_counter >= self.gc_stress_stride {
                self.gc_stress_counter = 0;
                if self.gc_stress_full {
                    // Ticks alternate between starting a concurrent cycle,
                    // which runs beside the next ticks' worth of mutation,
                    // and finishing it.
                    if !self.marking.is_marking() && self.gc_threads.workers() > 1 {
                        self.with_gc_pause_result(
                            GcPauseKind::MarkStart,
                            GcPauseTrigger::Stress,
                            |heap| heap.start_concurrent_marking_work(&mut allocation_roots),
                        )?;
                    } else {
                        self.in_major_gc = true;
                        let collect_result = self.collect_full_observed(
                            &mut allocation_roots,
                            GcPauseTrigger::Stress,
                            FullGcGoal::Throughput,
                        );
                        self.in_major_gc = false;
                        collect_result?;
                    }
                } else {
                    self.collect_minor_observed(&mut allocation_roots, GcPauseTrigger::Stress)?;
                }
            }
        }

        // Growth-ratio major GC bounds old-space occupancy before the
        // cage exhausts — independent of (and complementary to) the byte
        // cap, whose default equals the cage size and so never protects
        // against page-level exhaustion. O(1) page-count check.
        self.maybe_major_gc(&mut allocation_roots)?;

        let young = aligned <= LARGE_OBJECT_THRESHOLD;
        let mut scavenged = false;
        if young {
            let mut refill = self.refill_lab(aligned);
            if refill == LabRefill::NurseryFull && self.always_allocate_depth == 0 {
                self.collect_minor_observed(
                    &mut allocation_roots,
                    GcPauseTrigger::NurseryCapacity,
                )?;
                scavenged = true;
                refill = self.refill_lab(aligned);
            }
            if refill == LabRefill::Ready {
                let Some(address) = self.lab.bump(aligned) else {
                    unreachable!("a refilled buffer holds the cell it was sized for");
                };
                self.runtime_lab_bytes += aligned as u64;
                return Ok(self.publish_lab_cell(
                    address,
                    aligned,
                    value,
                    extra_bytes,
                    TrailingInit::Zeroed,
                    initialize,
                ));
            }
        }

        // Per-object placement. The buffer is retired here, so a direct
        // nursery bump cannot overlap it. Cap check — runs before any slot
        // is carved (architecture §2.1 caveat).
        if self.max_heap_bytes != 0 {
            self.account_or_collect_with_roots(aligned as u64, &mut allocation_roots)?;
        }

        let is_marking = self.marking.is_marking();

        // `placed_in_old` records that this object was tenured straight
        // into old-space (large object, or the nursery-deadlock overflow
        // below). Its header must then be born old, not young, or the
        // scavenger would try to evacuate an object sitting on an
        // old-space page.
        let mut placed_in_old = !young;
        // A large body the nursery admits is young, on its own page.
        let mut young_large = false;
        let offset = if !young {
            match self.admit_young_large(aligned, &mut allocation_roots)? {
                Some(offset) => {
                    placed_in_old = false;
                    young_large = true;
                    offset
                }
                None => self.large_space.alloc(aligned)?,
            }
        } else {
            // Try young-gen first; if full, scavenge then retry.
            match self.new_space.alloc(aligned) {
                Some(off) => off,
                None if self.always_allocate_depth != 0 || scavenged => {
                    placed_in_old = true;
                    self.help_old_sweep();
                    self.old_space.alloc(aligned)?
                }
                None => {
                    // Trigger scavenge with caller-supplied
                    // external roots; handle stack + globals are
                    // walked internally.
                    self.collect_minor_observed(
                        &mut allocation_roots,
                        GcPauseTrigger::NurseryCapacity,
                    )?;
                    match self.new_space.alloc(aligned) {
                        Some(off) => off,
                        // Young-gen deadlock: the live survivor set fills
                        // the semispace, so a copying scavenge frees no
                        // room for this allocation (it neither promotes —
                        // survivors have not yet aged — nor leaves a free
                        // slot). Rather than surface a spurious OOM while
                        // the cage still has pages, overflow this object
                        // straight into old-space. It is tenured a little
                        // early; the next scavenge drains the rest of the
                        // nursery normally. Guarantees forward progress
                        // for workloads that retain a nursery-sized live
                        // set (e.g. building a large array of objects).
                        None => {
                            placed_in_old = true;
                            self.help_old_sweep();
                            self.old_space.alloc(aligned)?
                        }
                    }
                }
            }
        };

        // Initialise header.
        // SAFETY: offset is a fresh in-cage allocation; pointer
        // arithmetic preserves provenance through the cage's
        // base pointer.
        let header_ptr = unsafe { cage_base().add(offset as usize) as *mut GcHeader };
        let payload_ptr =
            unsafe { cage_base().add(offset as usize + std::mem::size_of::<GcHeader>()) as *mut T };
        // SAFETY: offset references a freshly-alloc-ed cage
        // region inside a page we own. The header and payload are
        // fully initialised below before any read.
        unsafe {
            let header = if placed_in_old {
                GcHeader::new(T::TYPE_TAG, aligned as u32)
            } else {
                GcHeader::new_young(T::TYPE_TAG, aligned as u32)
            };
            // A young large cell's page is walked when the scavenge
            // releases it; only nursery cells are tracked.
            if !placed_in_old && !young_large && self.trace_table.needs_cleanup(T::TYPE_TAG) {
                self.new_space.track_cleanup(offset);
            }
            std::ptr::write(header_ptr, header);
            std::ptr::write(payload_ptr, value);
            if placed_in_old {
                self.marking.record_old_birth(header_ptr);
            }
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
            // The cell is still unreachable: fill any trailing storage at its
            // final address before the barrier scan below reads it.
            initialize(&mut *payload_ptr);
            crate::cell_map::debug_check_map_word(payload_ptr);
            // When this object was tenured straight into old-space (large
            // object, or the nursery-deadlock overflow), the `ptr::write`
            // bypassed the mutator write barrier. Any young children in the
            // payload are old→young edges the next scavenge can only find
            // via the card table, so barrier every edge now (mirrors
            // `alloc_old_with_roots_inner`). Young allocations need no card
            // — the scavenger walks them directly.
            if placed_in_old {
                let marking = &mut self.marking;
                let remembered = &mut self.remembered_parents;
                T::trace_slots(payload_ptr, &mut |slot| {
                    crate::barrier::write_barrier(header_ptr, *slot, marking, remembered);
                });
                if is_marking {
                    // As in `alloc_old_with_roots_inner`: the header is
                    // visible before any store can publish the cell.
                    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                }
            }
        }
        // Counter wiring inlined to keep the alloc fast path
        // tight. Only the per-tag row is touched (one cache
        // line, fixed offset since `T::TYPE_TAG` is const); the
        // aggregate `live_objects` / `live_bytes` are derived in
        // [`Self::gc_stats`] / [`Self::reconcile_live_counts`].
        // Wrapping arithmetic — overflow is unreachable in
        // practice (≥ 1.8 × 10¹⁹ allocs).
        self.count_runtime_alloc(T::TYPE_TAG, aligned);
        // SAFETY: offset is the cage offset of a freshly-alloc-ed
        // T payload; type tag matches `T::TYPE_TAG`.
        Ok(unsafe { Gc::from_offset(offset) })
    }
}
