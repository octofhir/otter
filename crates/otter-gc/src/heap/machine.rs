//! Nursery windows and fixed allocation groups for generated code.
//!
//! # Contents
//! - LAB policy, refill, publication and retirement.
//! - Rooted group admission, carving and initialized placement.
//!
//! # Invariants
//! - Generated windows use the heap-owned LAB and decline at policy boundaries.
//! - No group publishes uninitialized cells or hides a collecting fallback.
//!
//! # See also
//! - [`crate::lab`].

use super::{CellCarve, GcHeap, LabRefill, MachineAllocationWindow, RootSlotVisitor};
use crate::compressed::{Gc, cage_base};
use crate::header::GcHeader;
use crate::lab::LinearAllocationArea;
use crate::observation::GcPauseTrigger;
use crate::oom::OutOfMemory;
use crate::page::{CELL_SIZE, align_up};
use crate::trace::Traceable;

impl GcHeap {
    /// Whether ordinary allocations are being routed to old-space.
    #[must_use]
    pub const fn tenures_all(&self) -> bool {
        self.tenure_all
    }

    /// Route ordinary allocations to old-space until this is turned back off.
    ///
    /// Meant for a bootstrap phase whose objects are known to outlive every
    /// collection: allocating them young makes the first scavenges copy the
    /// whole set and every later one trace it again, for objects that were
    /// never going to die.
    pub fn set_tenure_all(&mut self, tenure_all: bool) {
        self.retire_machine_labs();
        self.tenure_all = tenure_all;
    }

    /// Publish the nursery window generated code allocates bodies from.
    ///
    /// The window names the heap's linear allocation buffer, which is empty
    /// whenever heap caps, stress collection, incremental marking or
    /// bootstrap tenuring need the ordinary rooted path, so generated code
    /// tests nothing but the bump. The buffer and statistics addresses are
    /// stable for the heap's lifetime. Callers register every body type the
    /// generated code carves ([`Self::register_traceable`]).
    #[doc(hidden)]
    pub fn machine_allocation_window(&mut self) -> MachineAllocationWindow {
        MachineAllocationWindow {
            lab: std::ptr::addr_of_mut!(self.lab),
            old_lab: std::ptr::addr_of_mut!(self.old_lab),
        }
    }

    /// Whether a refilled buffer may serve plain cells now: not under GC
    /// stress (every allocation reaches the stress counter) or during
    /// bootstrap tenuring. Both buffers stay live through a marking cycle:
    /// young cells are never marked, and an old cell carved while marking
    /// runs is born black, with the edges it starts with barriered.
    #[doc(hidden)]
    pub fn machine_allocation_allowed(&self) -> bool {
        !self.tenure_all && self.gc_stress_stride == 0
    }

    /// Whether [`Self::ensure_machine_allocation_with_roots`] would admit
    /// `aligned` bytes from the current buffer without any collection, so a
    /// caller can skip preparing roots for it.
    #[inline]
    #[must_use]
    pub fn machine_allocation_ready(&self, aligned: usize) -> bool {
        aligned != 0
            && aligned.is_multiple_of(CELL_SIZE)
            && self.machine_allocation_allowed()
            && self.always_allocate_depth == 0
            && self.lab.remaining() >= aligned
    }

    /// Publish both buffers' progress into their pages, so a page walk sees
    /// every cell handed out so far. Idempotent; the buffers stay live.
    pub(super) fn sync_lab(&self) {
        self.sync_old_lab();
        if self.lab_page == 0 {
            return;
        }
        let header = self.lab_page as *mut crate::page::PageHeader;
        // SAFETY: `lab_page` is the base of a live from-space page this heap
        // owns; the single mutator is the only writer of its header.
        unsafe {
            (*header).bump_cursor = self.lab.top - self.lab_page;
            (*header).allocated_bytes = self.lab_page_allocated + (self.lab.top - self.lab_start);
        }
    }

    /// Retire both machine buffers at a collection or policy boundary.
    pub(super) fn retire_machine_labs(&mut self) {
        self.retire_lab();
        self.retire_old_lab();
    }

    /// Publish the buffer into its page and drop it, refunding the unused
    /// tail to the heap cap.
    pub(super) fn retire_lab(&mut self) {
        if self.lab_page == 0 {
            return;
        }
        self.sync_lab();
        self.lab_consumed_bytes += (self.lab.top - self.lab_start) as u64;
        if self.lab_charged != 0 {
            self.tracked_bytes = self
                .tracked_bytes
                .saturating_sub(self.lab.remaining() as u64);
            self.lab_charged = 0;
        }
        self.lab = LinearAllocationArea::EMPTY;
        self.lab_page = 0;
        self.lab_start = 0;
        self.lab_page_allocated = 0;
    }

    /// Retire the buffer and carve a fresh one with room for `aligned`
    /// bytes from the nursery. Never collects.
    pub(super) fn refill_lab(&mut self, aligned: usize) -> LabRefill {
        self.retire_lab();
        if !self.machine_allocation_allowed() {
            return LabRefill::Declined;
        }
        let Some((page, start, mut limit)) = self.new_space.take_tail(aligned) else {
            return LabRefill::NurseryFull;
        };
        if self.max_heap_bytes != 0 {
            // Charge the whole window now so a bump never accounts; near the
            // cap the window shrinks to what the cap still admits.
            self.drain_shared_external_releases();
            let available = self.max_heap_bytes.saturating_sub(self.tracked_bytes);
            let extent = ((limit - start) as u64).min(available) as usize & !(CELL_SIZE - 1);
            if extent < aligned {
                return LabRefill::Declined;
            }
            limit = start + extent;
            self.tracked_bytes += extent as u64;
            self.lab_charged = extent as u64;
        }
        // SAFETY: `page` is the base of a live from-space page.
        self.lab_page_allocated =
            unsafe { (*(page as *const crate::page::PageHeader)).allocated_bytes };
        self.lab_page = page;
        self.lab_start = start;
        self.lab = LinearAllocationArea { top: start, limit };
        LabRefill::Ready
    }

    /// Ensure room for a generated fixed allocation group in the one LAB.
    ///
    /// No cell is carved or returned here. Success leaves an initialized
    /// buffer; generated code reloads its owner and current operands after
    /// the safepoint, initializes every group cell, then publishes top once.
    /// Disabled policy or group-size admission returns false without an OOM
    /// latch. The caller resumes the FIRST original allocation so a smaller
    /// legal prefix can succeed before the actual source OOM, if any.
    ///
    /// # Errors
    /// Collector/cage failures precede forwarding or leave a completed heap
    /// operation. Canonical roots remain valid for first-source recovery.
    #[doc(hidden)]
    pub fn ensure_machine_allocation_with_roots(
        &mut self,
        aligned: usize,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<bool, OutOfMemory> {
        if aligned == 0
            || !aligned.is_multiple_of(CELL_SIZE)
            || aligned > crate::page::PAGE_PAYLOAD_SIZE
            || !self.machine_allocation_allowed()
            || self.always_allocate_depth != 0
        {
            return Ok(false);
        }
        if self.lab.remaining() >= aligned {
            return Ok(true);
        }
        // Publish existing objects and refund only the already-admitted unused
        // tail. A failed search leaves the nursery cursor at its prior page,
        // so that same tail remains usable by the first smaller source cell.
        self.retire_lab();
        self.maybe_major_gc(external_visit)?;
        if !self.machine_allocation_allowed() {
            return Ok(false);
        }
        if !self.can_admit_with_roots(aligned as u64, external_visit)? {
            return Ok(false);
        }
        if !self.machine_allocation_allowed() {
            return Ok(false);
        }
        match self.refill_lab(aligned) {
            LabRefill::Ready => return Ok(true),
            LabRefill::Declined => return Ok(false),
            LabRefill::NurseryFull => {}
        }
        self.collect_minor_observed(external_visit, GcPauseTrigger::NurseryCapacity)?;
        if !self.machine_allocation_allowed() {
            return Ok(false);
        }
        if !self.can_admit_with_roots(aligned as u64, external_visit)? {
            return Ok(false);
        }
        if !self.machine_allocation_allowed() {
            return Ok(false);
        }
        Ok(self.refill_lab(aligned) == LabRefill::Ready)
    }

    /// Reserve `aligned` bytes the nursery buffer already holds for a group
    /// of cells, each placed by [`Self::place`]: the sum of their
    /// [`Self::cell_bytes`]. One bump serves the whole group, and nothing
    /// here collects or moves a cell. `None` when the buffer is not ready
    /// ([`Self::machine_allocation_ready`]). A carve dropped with bytes
    /// unplaced leaves them a free-space filler. The group is accounted as
    /// generated allocation is: by the buffer's progress, not per type.
    #[inline(always)]
    pub fn carve(&mut self, aligned: usize) -> Option<CellCarve> {
        if !self.machine_allocation_ready(aligned) {
            return None;
        }
        let address = self.lab.bump(aligned)?;
        Some(CellCarve {
            next: address,
            end: address + aligned,
        })
    }

    /// Place the group's next cell: `value` followed by `extra_bytes` of
    /// trailing storage that `initialize` writes whole, as
    /// [`Self::try_alloc_filled_trailing_no_collect_or_return`] would. `T`
    /// must have been allocated before, so its trace entry exists.
    ///
    /// # Panics
    /// When the cell does not fit the carve.
    #[inline(always)]
    pub fn place<T: Traceable>(
        &mut self,
        carve: &mut CellCarve,
        value: T,
        extra_bytes: usize,
        initialize: impl FnOnce(&mut T),
    ) -> Gc<T> {
        let aligned = Self::cell_bytes::<T>(extra_bytes);
        let address = carve.next;
        assert!(
            address + aligned <= carve.end,
            "a group's cells exceed the bytes carved for them"
        );
        debug_assert!(
            self.trace_table.get(T::TYPE_TAG).is_some(),
            "a placed type was never allocated"
        );
        carve.next += aligned;
        let offset = address as u32;
        // SAFETY: the carve handed out `aligned` bytes inside a live
        // from-space page; header and payload are fully written before the
        // handle is published, with no safepoint in between.
        unsafe {
            let header_ptr = cage_base().add(offset as usize).cast::<GcHeader>();
            let payload_ptr = header_ptr
                .cast::<u8>()
                .add(std::mem::size_of::<GcHeader>())
                .cast::<T>();
            // The cell's last word holds any alignment padding; the
            // initializer writes every trailing byte before it.
            if extra_bytes != 0 {
                header_ptr
                    .cast::<u8>()
                    .add(aligned - CELL_SIZE)
                    .cast::<u64>()
                    .write_unaligned(0);
            }
            std::ptr::write(header_ptr, GcHeader::new_young(T::TYPE_TAG, aligned as u32));
            std::ptr::write(payload_ptr, value);
            initialize(&mut *payload_ptr);
            crate::cell_map::debug_check_map_word(payload_ptr);
        }
        if self.trace_table.needs_cleanup(T::TYPE_TAG) {
            self.new_space.track_cleanup(offset);
        }
        // SAFETY: offset is the cage offset of a freshly written `T` payload.
        unsafe { Gc::from_offset(offset) }
    }

    /// Bytes a `T` cell with `extra_bytes` of trailing storage takes from an
    /// allocation buffer: the unit [`Self::ensure_machine_allocation_with_roots`]
    /// admits, so a group of cells reserved together is the sum of these.
    #[must_use]
    pub const fn cell_bytes<T: Traceable>(extra_bytes: usize) -> usize {
        align_up(
            std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>() + extra_bytes,
            CELL_SIZE,
        )
    }
}
