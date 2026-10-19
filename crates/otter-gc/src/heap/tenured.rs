//! Site-attributed placement: tagged young cells and the old-space buffer.
//!
//! # Contents
//! - [`SitePlacement`] — where one site-attributed allocation lands.
//! - [`GcHeap::alloc_trailing_at_site_with_roots_initialized`] — runtime
//!   allocation for an allocation site.
//! - The old-space linear allocation buffer generated code and the runtime
//!   share for tenured sites: refill, publication into its page, retirement.
//!
//! # Invariants
//! - Only a young cell carries a site tag, and only a tracking site tags.
//! - The old buffer exists only while plain machine allocation is legal: never
//!   while marking, under stress or during bootstrap tenuring. Every
//!   collection retires it first, so a mark never meets an unpublished
//!   old cell and no old cell is born white during a mark.
//! - The whole buffer is booked on its page and against the cap at refill.
//!   Its unused tail always starts with a free filler after a sync or a
//!   retirement; retirement returns that tail to the page and free list and
//!   refunds it to the cap.
//! - An old cell records every young child it was initialized with in the
//!   remembered set before any collection can observe it.
//!
//! # See also
//! - [`crate::pretenuring`] owns survival feedback and decisions.
//! - [`crate::scavenger`] counts and clears site tags.

use super::{GcHeap, RootSlotVisitor};
use crate::compressed::{Gc, RawGc, cage_base};
use crate::header::GcHeader;
use crate::lab::LinearAllocationArea;
use crate::oom::OutOfMemory;
use crate::page::{CELL_SIZE, LARGE_OBJECT_THRESHOLD, align_up};
use crate::trace::Traceable;

/// Bytes one old-space buffer refill asks for beyond the requesting cell.
pub const OLD_LAB_BYTES: usize = 32 * 1024;

/// Where one site-attributed allocation lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SitePlacement {
    /// The nursery, tagged with a tracking site (zero: untagged).
    Young(u16),
    /// Old space directly.
    Old,
}

impl GcHeap {
    /// The isolate's allocation sites.
    #[must_use]
    pub fn allocation_sites(&self) -> &crate::pretenuring::AllocationSites {
        &self.allocation_sites
    }

    /// Mutable access to the isolate's allocation sites.
    pub fn allocation_sites_mut(&mut self) -> &mut crate::pretenuring::AllocationSites {
        &mut self.allocation_sites
    }

    /// [`Self::alloc_trailing_with_roots_initialized`] for an allocation site.
    ///
    /// A young cell of a tracking site is tagged and counted as created; a
    /// tenured site's cell is born old with its young children remembered.
    ///
    /// # Errors
    ///
    /// Same as [`Self::alloc`].
    pub fn alloc_trailing_at_site_with_roots_initialized<T: Traceable>(
        &mut self,
        value: T,
        extra_bytes: usize,
        placement: SitePlacement,
        external_visit: &mut RootSlotVisitor<'_>,
        initialize: impl FnOnce(&mut T),
    ) -> Result<Gc<T>, OutOfMemory> {
        match placement {
            SitePlacement::Young(site) => {
                let cell = self.alloc_trailing_with_roots_inner(
                    value,
                    extra_bytes,
                    external_visit,
                    initialize,
                )?;
                if site != 0 {
                    let header = cell.as_header_ptr();
                    // SAFETY: the cell was just allocated and is not yet
                    // observable by any collection.
                    unsafe {
                        if (*header).is_young() {
                            GcHeader::set_allocation_site(header, site);
                            self.allocation_sites.record_created(site);
                        }
                    }
                }
                Ok(cell)
            }
            SitePlacement::Old => {
                self.alloc_tenured_trailing(value, extra_bytes, external_visit, initialize)
            }
        }
    }

    fn alloc_tenured_trailing<T: Traceable>(
        &mut self,
        mut value: T,
        extra_bytes: usize,
        external_visit: &mut RootSlotVisitor<'_>,
        initialize: impl FnOnce(&mut T),
    ) -> Result<Gc<T>, OutOfMemory> {
        if self.trace_table.get(T::TYPE_TAG).is_none() {
            self.trace_table.register::<T>();
        }
        let total = std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>() + extra_bytes;
        let aligned = align_up(total, CELL_SIZE);
        if aligned <= LARGE_OBJECT_THRESHOLD
            && let Some(address) = self.old_lab.bump(aligned)
        {
            return Ok(self.publish_old_lab_cell(address, aligned, value, extra_bytes, initialize));
        }
        if aligned <= LARGE_OBJECT_THRESHOLD && self.machine_allocation_allowed() {
            let pending_value = std::ptr::addr_of_mut!(value);
            let mut allocation_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                external_visit(visitor);
                // SAFETY: `pending_value` is the unpublished payload on this
                // frame; a collection may rewrite its slots before the copy.
                unsafe { T::trace_pending_slots(pending_value, visitor) };
            };
            self.maybe_major_gc(&mut allocation_roots)?;
            if self.machine_allocation_allowed() && self.refill_old_lab(aligned) {
                let Some(address) = self.old_lab.bump(aligned) else {
                    unreachable!("a refilled old buffer holds the cell it was sized for");
                };
                return Ok(self.publish_old_lab_cell(
                    address,
                    aligned,
                    value,
                    extra_bytes,
                    initialize,
                ));
            }
        }
        self.alloc_old_with_roots_inner(value, true, extra_bytes, external_visit, initialize)
    }

    /// Write an old cell carved from the old buffer at `address`.
    fn publish_old_lab_cell<T: Traceable>(
        &mut self,
        address: usize,
        aligned: usize,
        value: T,
        extra_bytes: usize,
        initialize: impl FnOnce(&mut T),
    ) -> Gc<T> {
        let offset = address as u32;
        debug_assert_eq!(address - cage_base() as usize, offset as usize);
        // SAFETY: the old buffer handed out `aligned` bytes inside a live old
        // page; header and payload are written before any safepoint, and the
        // initialized edges are remembered before the handle escapes.
        unsafe {
            let header_ptr = cage_base().add(offset as usize).cast::<GcHeader>();
            let payload_ptr = header_ptr
                .cast::<u8>()
                .add(std::mem::size_of::<GcHeader>())
                .cast::<T>();
            std::ptr::write(header_ptr, GcHeader::new(T::TYPE_TAG, aligned as u32));
            std::ptr::write(payload_ptr, value);
            if extra_bytes != 0 {
                let tail = payload_ptr.cast::<u8>().add(std::mem::size_of::<T>());
                std::ptr::write_bytes(
                    tail,
                    0,
                    aligned - std::mem::size_of::<GcHeader>() - std::mem::size_of::<T>(),
                );
            }
            initialize(&mut *payload_ptr);
            crate::cell_map::debug_check_map_word(payload_ptr);
            self.marking.record_old_lab_birth(header_ptr);
            let marking = &mut self.marking;
            let remembered = &mut self.remembered_parents;
            T::trace_slots(payload_ptr, &mut |slot| {
                crate::barrier::write_barrier(header_ptr, *slot, marking, remembered);
            });
            if self.marking.is_marking() {
                // The concurrent marker may reach the cell once a store
                // publishes it: its initialized header must be visible first.
                std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
            }
        }
        let row = &mut self.gc_stats.by_type[T::TYPE_TAG as usize];
        row.live_bytes = row.live_bytes.wrapping_add(aligned);
        row.live_count = row.live_count.wrapping_add(1);
        row.alloc_count_total = row.alloc_count_total.wrapping_add(1);
        row.alloc_bytes_total = row
            .alloc_bytes_total
            .wrapping_add(u64::try_from(aligned).unwrap_or(u64::MAX));
        // SAFETY: offset is the cage offset of a freshly written `T` payload.
        unsafe { Gc::from_offset(offset) }
    }

    /// Retire the old buffer and carve a fresh one holding at least
    /// `aligned` bytes. Never collects; `false` leaves the buffer empty.
    pub(super) fn refill_old_lab(&mut self, aligned: usize) -> bool {
        self.retire_old_lab();
        if !self.machine_allocation_allowed() {
            return false;
        }
        let mut extent = OLD_LAB_BYTES.max(aligned);
        if self.max_heap_bytes != 0 {
            self.drain_shared_external_releases();
            let available = self.max_heap_bytes.saturating_sub(self.tracked_bytes);
            extent = (extent as u64).min(available) as usize & !(CELL_SIZE - 1);
            if extent < aligned {
                return false;
            }
        }
        // Any swept hole the request fits serves as the buffer, as it would
        // for a direct old allocation.
        let Ok((offset, extent)) = self.old_space.take_lab(aligned, extent, extent) else {
            return false;
        };
        if self.max_heap_bytes != 0 {
            self.tracked_bytes += extent as u64;
            self.old_lab_charged = extent as u64;
        }
        let start = cage_base() as usize + offset as usize;
        // SAFETY: `[start, start + extent)` was just carved from a live old
        // page for this buffer alone; a free filler keeps the page walkable.
        unsafe {
            std::ptr::write(start as *mut GcHeader, GcHeader::new_free(extent as u32));
            // Cells carved from it while marking runs are born black and
            // never visited: the whole buffer counts toward its page's live
            // bytes, so the sweep keeps the page and judges each cell.
            if self.marking.is_marking() {
                self.marking.count_live_extent(start as *mut GcHeader);
            }
        }
        self.old_lab = LinearAllocationArea {
            top: start,
            limit: start + extent,
        };
        true
    }

    /// Cap the old buffer's unused tail with a free filler, so a page walk
    /// sees exactly the cells handed out so far. The buffer stays live.
    pub(super) fn sync_old_lab(&self) {
        let LinearAllocationArea { top, limit } = self.old_lab;
        if top < limit {
            // SAFETY: `[top, limit)` is the unused tail of a buffer carved
            // from a live old page; fillers are never traced.
            unsafe {
                std::ptr::write(
                    top as *mut GcHeader,
                    GcHeader::new_free((limit - top) as u32),
                );
            }
        }
    }

    /// Publish the old buffer and return its unused tail to its page, the
    /// free list and the cap.
    pub(super) fn retire_old_lab(&mut self) {
        let LinearAllocationArea { top, limit } = self.old_lab;
        if limit == 0 {
            return;
        }
        self.sync_old_lab();
        let unused = limit - top;
        if unused != 0 {
            let offset = (top - cage_base() as usize) as u32;
            // The page booked the whole buffer at refill.
            self.old_space.return_lab(offset, unused);
        }
        if self.old_lab_charged != 0 {
            self.tracked_bytes = self.tracked_bytes.saturating_sub(unused as u64);
            self.old_lab_charged = 0;
        }
        self.old_lab = LinearAllocationArea::EMPTY;
    }
}

#[cfg(test)]
#[path = "tenured_tests.rs"]
mod tests;
