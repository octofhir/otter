//! Heap-cap admission, external reservations and allocation counters.
//!
//! # Contents
//! - Charged and effective LAB/external accounting.
//! - Rooted emergency collection, reservation rollback and statistics.
//!
//! # Invariants
//! - Only completed allocations and reservations book bytes.
//! - All collecting admission paths expose the caller roots before movement.
//!
//! # See also
//! - [`super::allocation_limits`].

use super::{GcHeap, HeapStats, RootSlotVisitor};
use crate::collection_goal::FullGcGoal;
use crate::compressed::RawGc;
use crate::external::{ExternalMemory, SharedExternalMemory, SharedExternalState};
use crate::observation::{GcPauseKind, GcPauseOutcome, GcPauseTrigger};
use crate::oom::OutOfMemory;
use crate::stats::GcStats;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

impl GcHeap {
    /// Configured per-heap cap in bytes (`0` = disabled).
    pub fn max_heap_bytes(&self) -> u64 {
        self.max_heap_bytes
    }

    /// Bytes currently tracked against the cap (slot allocations
    /// + outstanding [`Self::reserve_bytes`] reservations).
    pub fn tracked_bytes(&self) -> u64 {
        self.effective_tracked_bytes()
    }

    /// Shared release channel handed to external reservation tokens.
    pub(crate) fn shared_external_state(&self) -> Arc<SharedExternalState> {
        Arc::clone(&self.shared_external)
    }

    pub(super) fn pending_shared_external_releases(&self) -> u64 {
        self.shared_external
            .released_bytes()
            .min(self.reserved_bytes)
    }

    pub(super) fn effective_reserved_bytes(&self) -> u64 {
        self.reserved_bytes
            .saturating_sub(self.pending_shared_external_releases())
    }

    pub(super) fn effective_tracked_bytes(&self) -> u64 {
        if self.max_heap_bytes == 0 {
            return self.tracked_bytes;
        }
        // The buffer's unused tail is charged but not yet handed out.
        let unused_lab = if self.lab_charged == 0 {
            0
        } else {
            self.lab.remaining() as u64
        } + if self.old_lab_charged == 0 {
            0
        } else {
            self.old_lab.remaining() as u64
        };
        self.tracked_bytes
            .saturating_sub(self.pending_shared_external_releases())
            .saturating_sub(unused_lab)
    }

    pub(super) fn drain_shared_external_releases(&mut self) {
        let released = self.shared_external.take_released_bytes();
        if released == 0 {
            return;
        }
        let actual = released.min(self.reserved_bytes);
        self.reserved_bytes = self.reserved_bytes.saturating_sub(actual);
        if self.max_heap_bytes != 0 {
            self.tracked_bytes = self.tracked_bytes.saturating_sub(actual);
        }
        self.shrink_external_ledger(actual);
    }

    /// Reconcile deferred external-token releases now.
    ///
    /// Token drops record their byte counts in the shared release channel;
    /// every accounting operation drains it implicitly. This explicit hook
    /// gives hosts and tests a deterministic reconciliation point without
    /// booking anything.
    pub fn drain_external_releases(&mut self) {
        self.drain_shared_external_releases();
    }

    /// Install (or replace) the shared runtime ledger mirroring this heap's
    /// outstanding external/off-slot reservation bytes as `ExternalBytes`.
    /// The currently outstanding bytes are charged immediately; replacing the
    /// account releases the previous account's charge in full.
    ///
    /// # Errors
    /// Returns the ledger refusal without changing either ledger.
    pub fn set_external_bytes_account(
        &mut self,
        account: &otter_resource::ResourceAccount,
    ) -> Result<(), otter_resource::ResourceError> {
        self.drain_shared_external_releases();
        let lease = account.reserve_exact(
            otter_resource::ResourceClass::ExternalBytes,
            self.reserved_bytes,
        )?;
        self.external_ledger = Some(lease);
        Ok(())
    }

    /// Admit `bytes` more outstanding external bytes on the installed shared
    /// ledger. Runs before any heap-side booking, so a refusal is typed and
    /// changes nothing. The heap-local OOM flag stays clear: an aggregate
    /// budget refusal is not this isolate's heap exhaustion.
    pub(super) fn admit_external_growth(&mut self, bytes: u64) -> Result<(), OutOfMemory> {
        let Some(lease) = self.external_ledger.as_mut() else {
            return Ok(());
        };
        let charged = lease.amount();
        let target = charged.saturating_add(bytes);
        match lease.resize(target) {
            Ok(()) => Ok(()),
            Err(error) => {
                // The lease-replacement error reports usage *independent of*
                // this lease; add the lease's own charge back so the refusal
                // names the full outstanding total.
                let (in_use, limit) = match error {
                    otter_resource::ResourceError::Exhausted { in_use, limit, .. } => {
                        (in_use.saturating_add(charged), limit)
                    }
                    otter_resource::ResourceError::Overflow { in_use, limit, .. } => {
                        (in_use.saturating_add(charged), limit.unwrap_or(u64::MAX))
                    }
                    otter_resource::ResourceError::Allocation { .. } => {
                        unreachable!(
                            "ResourceLease::resize only updates the fixed ledger and never allocates"
                        )
                    }
                };
                Err(OutOfMemory::ExternalBudgetExceeded {
                    requested_bytes: bytes,
                    in_use,
                    limit,
                })
            }
        }
    }

    /// Shrink the installed ledger mirror by exactly `bytes`, following a
    /// heap-side release or a rolled-back admission.
    pub(super) fn shrink_external_ledger(&mut self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        if let Some(lease) = self.external_ledger.as_mut() {
            let target = lease.amount().saturating_sub(bytes);
            let shrunk = lease.resize(target);
            debug_assert!(shrunk.is_ok(), "shrinking an external lease cannot fail");
        }
    }

    /// Cooperative-cancellation OOM flag. Cloned cheaply; safe to
    /// share with watchdogs / interrupt threads. Never the
    /// **primary** OOM signal — see [`Self::alloc`].
    pub fn oom_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.oom_flag)
    }

    /// Reserve `bytes` against the cap without materialising a
    /// slot (off-slot accounting — e.g. `Vec` capacity inside a
    /// payload).
    ///
    /// Runs the growth-triggered major GC first when the major-GC
    /// budget (pages plus outstanding reservations) is due. On cap
    /// overshoot: one emergency full GC, then retry once. If still
    /// over → [`OutOfMemory::HeapCapExceeded`] and the reservation is
    /// **not** booked.
    ///
    /// # Errors
    ///
    /// [`OutOfMemory::HeapCapExceeded`] when the cap is enabled
    /// and the reservation cannot be satisfied even after an
    /// emergency full GC.
    pub fn reserve_bytes(&mut self, bytes: u64) -> Result<(), OutOfMemory> {
        self.drain_shared_external_releases();
        let mut noop = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.maybe_major_gc(&mut noop)?;
        self.admit_external_growth(bytes)?;
        if self.max_heap_bytes == 0 {
            self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
            return Ok(());
        }
        if bytes > self.max_heap_bytes {
            self.shrink_external_ledger(bytes);
            self.oom_flag.store(true, Ordering::Relaxed);
            return Err(OutOfMemory::HeapCapExceeded {
                requested_bytes: bytes,
                heap_limit_bytes: self.max_heap_bytes,
            });
        }
        if let Err(error) = self.account_or_collect(bytes) {
            self.shrink_external_ledger(bytes);
            return Err(error);
        }
        self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
        Ok(())
    }

    /// Reserve `bytes` against the cap while exposing caller-owned roots to a
    /// possible emergency full GC.
    ///
    /// Use this for off-slot backing storage that is being prepared by a
    /// mutator stack frame whose values are not all present in heap
    /// handle/global tables. The growth-triggered major GC, when due, and
    /// any emergency collection both see those roots. The reservation is
    /// booked only after the cap check succeeds.
    ///
    /// # Errors
    ///
    /// [`OutOfMemory::HeapCapExceeded`] when the cap is enabled and the
    /// reservation cannot be satisfied even after an emergency full GC that
    /// sees `external_visit`.
    pub fn reserve_bytes_with_roots(
        &mut self,
        bytes: u64,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.drain_shared_external_releases();
        self.maybe_major_gc(external_visit)?;
        self.admit_external_growth(bytes)?;
        if self.max_heap_bytes == 0 {
            self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
            return Ok(());
        }
        if bytes > self.max_heap_bytes {
            self.shrink_external_ledger(bytes);
            self.oom_flag.store(true, Ordering::Relaxed);
            return Err(OutOfMemory::HeapCapExceeded {
                requested_bytes: bytes,
                heap_limit_bytes: self.max_heap_bytes,
            });
        }
        if let Err(error) = self.account_or_collect_with_roots(bytes, external_visit) {
            self.shrink_external_ledger(bytes);
            return Err(error);
        }
        self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
        Ok(())
    }

    /// Reserve native / backing-store bytes with RAII release.
    ///
    /// Use this for memory not represented by GC cell sizes: typed
    /// array buffers, host-owned native allocations, module source
    /// caches, or container backing storage.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfMemory`] when the configured cap cannot
    /// accommodate the reservation after one emergency collection.
    pub fn reserve_external(&mut self, bytes: u64) -> Result<ExternalMemory, OutOfMemory> {
        ExternalMemory::new(self, bytes)
    }

    /// Reserve native / backing-store bytes while exposing caller-owned roots
    /// to any emergency collection.
    pub fn reserve_external_with_roots(
        &mut self,
        bytes: u64,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<ExternalMemory, OutOfMemory> {
        ExternalMemory::new_with_roots(self, bytes, external_visit)
    }

    /// Reserve shared native / backing-store bytes while exposing caller-owned
    /// roots to any emergency collection.
    ///
    /// The returned token may be dropped from any thread; release accounting is
    /// reconciled by the owning heap on later accounting/stat calls.
    pub fn reserve_shared_external_with_roots(
        &mut self,
        bytes: u64,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<SharedExternalMemory, OutOfMemory> {
        self.reserve_bytes_with_roots(bytes, external_visit)?;
        Ok(SharedExternalMemory::new(
            Arc::clone(&self.shared_external),
            bytes,
        ))
    }

    /// Reserve off-slot bytes without running an emergency
    /// collection.
    ///
    /// Admission cannot collect. Live cell bytes and existing reservations
    /// consume the cap; an unused charged LAB tail is refunded only when it
    /// would otherwise reject growth that the effective cap admits. Ordinary
    /// reservations that fit the raw charge retain the current LAB.
    ///
    /// # Errors
    ///
    /// [`OutOfMemory::HeapCapExceeded`] when `bytes` would exceed
    /// the configured cap.
    pub fn reserve_bytes_no_collect(&mut self, bytes: u64) -> Result<(), OutOfMemory> {
        self.drain_shared_external_releases();
        self.admit_external_growth(bytes)?;
        if self.max_heap_bytes == 0 {
            self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
            return Ok(());
        }
        // The public cap usage excludes bytes reserved by a LAB but not yet
        // handed to any cell. Preserve a live buffer on the ordinary admitted
        // path; only an otherwise false refusal publishes/refunds its tail.
        if self.tracked_bytes.saturating_add(bytes) > self.max_heap_bytes
            && self.effective_tracked_bytes().saturating_add(bytes) <= self.max_heap_bytes
        {
            self.retire_machine_labs();
        }
        let projected = self.tracked_bytes.saturating_add(bytes);
        if projected > self.max_heap_bytes {
            self.shrink_external_ledger(bytes);
            self.oom_flag.store(true, Ordering::Relaxed);
            return Err(OutOfMemory::HeapCapExceeded {
                requested_bytes: bytes,
                heap_limit_bytes: self.max_heap_bytes,
            });
        }
        self.tracked_bytes = projected;
        self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
        Ok(())
    }

    /// Release `bytes` previously booked via [`Self::reserve_bytes`].
    /// Saturates at zero if the caller overshoots — not an error;
    /// keeps the counter monotone-correct under arithmetic edge
    /// cases.
    pub fn release_bytes(&mut self, bytes: u64) {
        self.drain_shared_external_releases();
        let actual = bytes.min(self.reserved_bytes);
        self.reserved_bytes = self.reserved_bytes.saturating_sub(actual);
        if self.max_heap_bytes != 0 {
            self.tracked_bytes = self.tracked_bytes.saturating_sub(actual);
        }
        self.shrink_external_ledger(actual);
    }

    /// Caller pre-checks `self.max_heap_bytes != 0`; this
    /// function is not invoked when the cap is disabled, so the
    /// disabled-cap branch never executes here.
    /// Rooted, noncharging admission. Only the actual reservation/allocation
    /// owner charges bytes after this check. Speculative allocation groups
    /// may refuse without poisoning the source's legal allocation prefix.
    pub(super) fn can_admit_with_roots(
        &mut self,
        bytes: u64,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<bool, OutOfMemory> {
        self.drain_shared_external_releases();
        let cap = self.max_heap_bytes;
        if cap == 0 {
            return Ok(true);
        }
        // A live LAB may have admitted unused tail bytes. Only a false raw
        // refusal publishes/refunds that tail; ordinary within-cap admission
        // keeps the buffer. The allocation owner charges once after success.
        if self.tracked_bytes.saturating_add(bytes) > cap
            && self.effective_tracked_bytes().saturating_add(bytes) <= cap
        {
            self.retire_machine_labs();
        }
        if self.tracked_bytes.saturating_add(bytes) <= cap {
            return Ok(true);
        }
        // A sweep still in progress charges its dead cells until it ends.
        if self.sweeping.is_some() {
            self.complete_old_sweep();
            if self.tracked_bytes.saturating_add(bytes) <= cap {
                return Ok(true);
            }
        }
        if self.always_allocate_depth != 0 {
            return Ok(false);
        }
        self.with_gc_pause(
            GcPauseKind::Full,
            GcPauseTrigger::HeapCap,
            |heap| match heap.collect_full_observed(
                external_visit,
                GcPauseTrigger::HeapCap,
                FullGcGoal::ReleaseMemory,
            ) {
                Err(error) => (Err(error), GcPauseOutcome::CollectionFailed),
                Ok(()) => {
                    let admitted = heap.tracked_bytes.saturating_add(bytes) <= cap;
                    (
                        Ok(admitted),
                        if admitted {
                            GcPauseOutcome::Completed
                        } else {
                            GcPauseOutcome::CompletedAllocationRefused
                        },
                    )
                }
            },
        )
    }

    /// Canonical charged admission for a cap-enabled allocation owner.
    /// Callers precheck `max_heap_bytes != 0`; speculative group admission uses
    /// the noncharging check above even with the cap disabled. On overshoot,
    /// perform one emergency full GC and retry once before actual refusal.
    #[cold]
    #[inline(never)]
    pub(super) fn account_or_collect_with_roots(
        &mut self,
        bytes: u64,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        if self.can_admit_with_roots(bytes, external_visit)? {
            self.tracked_bytes = self.tracked_bytes.saturating_add(bytes);
            return Ok(());
        }
        self.oom_flag.store(true, Ordering::Relaxed);
        Err(OutOfMemory::HeapCapExceeded {
            requested_bytes: bytes,
            heap_limit_bytes: self.max_heap_bytes,
        })
    }

    #[cold]
    #[inline(never)]
    pub(super) fn account_or_collect(&mut self, bytes: u64) -> Result<(), OutOfMemory> {
        // External root visitor is empty — the handle stack and
        // global handle table are walked from inside
        // `collect_full`, and the embedder has no opportunity to
        // re-enter the heap during a refused allocation.
        let mut noop = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.account_or_collect_with_roots(bytes, &mut noop)
    }

    /// Stats snapshot.
    pub fn stats(&self) -> HeapStats {
        self.sync_lab();
        let mut s = self.stats;
        s.new_allocated_bytes = self.new_space.allocated_bytes();
        s.old_allocated_bytes = self.old_space.allocated_bytes();
        s.allocated_bytes = s.new_allocated_bytes
            + s.old_allocated_bytes
            + self.large_space.allocated_bytes()
            + self.young_large_space.allocated_bytes();
        s.page_count = self.new_space.from_page_count() * 2
            + self.old_space.page_count()
            + self.large_space.page_count()
            + self.young_large_space.page_count();
        s.tracked_bytes = self.effective_tracked_bytes();
        s.reserved_bytes = self.effective_reserved_bytes();
        s.max_heap_bytes = self.max_heap_bytes;
        s
    }

    /// Bytes the old generation's cells occupy: old-space cells, live or not
    /// yet swept, plus whole large-object pages. O(1).
    pub(super) fn old_generation_consumed_bytes(&self) -> u64 {
        (self.old_space.allocated_bytes() as u64).saturating_add(
            (self.large_space.page_count() as u64).saturating_mul(crate::page::PAGE_SIZE as u64),
        )
    }

    /// Old/large page bytes: the old generation's occupancy.
    pub(super) fn old_generation_page_bytes(&self) -> u64 {
        (self.old_space.page_count() as u64)
            .saturating_add(self.large_space.page_count() as u64)
            .saturating_mul(crate::page::PAGE_SIZE as u64)
    }

    /// `(minor cycles, full cycles)` completed so far.
    ///
    /// Reads the two maintained counters directly; unlike [`Self::gc_stats`]
    /// it does not re-aggregate the per-type table, so a hot allocation path
    /// may sample it before and after one operation to learn whether a
    /// collection ran in between.
    #[must_use]
    pub fn gc_cycle_counts(&self) -> (u64, u64) {
        (self.gc_stats.minor_gc_cycles, self.gc_stats.gc_cycles)
    }

    /// Cumulative allocation and collection counters for an accounting
    /// checkpoint: `(objects allocated, bytes allocated, live bytes as of
    /// the last completed collection)`. Unlike [`Self::gc_stats`] it leaves a
    /// lazy old-space sweep in progress, so a frequent checkpoint costs no
    /// collection work.
    #[must_use]
    pub fn allocation_counters(&self) -> (u64, u64, usize) {
        (
            self.rows_alloc_count,
            self.rows_alloc_bytes.saturating_add(self.generated_bytes()),
            self.rows_live_bytes,
        )
    }

    /// `(tracked bytes, external reserved bytes)` as [`Self::stats`] reports
    /// them, without syncing the allocation buffers or walking pages.
    #[must_use]
    pub fn byte_counters(&self) -> (u64, u64) {
        (
            self.effective_tracked_bytes(),
            self.effective_reserved_bytes(),
        )
    }

    /// Book one runtime allocation of `aligned` bytes under `tag` in its
    /// per-type row and the row sums.
    #[inline(always)]
    pub(crate) fn count_runtime_alloc(&mut self, tag: u8, aligned: usize) {
        let bytes = u64::try_from(aligned).unwrap_or(u64::MAX);
        let row = &mut self.gc_stats.by_type[tag as usize];
        row.live_bytes = row.live_bytes.wrapping_add(aligned);
        row.live_count = row.live_count.wrapping_add(1);
        row.alloc_count_total = row.alloc_count_total.wrapping_add(1);
        row.alloc_bytes_total = row.alloc_bytes_total.wrapping_add(bytes);
        self.rows_alloc_count = self.rows_alloc_count.wrapping_add(1);
        self.rows_alloc_bytes = self.rows_alloc_bytes.wrapping_add(bytes);
        self.rows_live_bytes = self.rows_live_bytes.wrapping_add(aligned);
    }

    /// Cumulative collection counters: `(full cycles, minor cycles, slots the
    /// minor collections scanned)`.
    #[must_use]
    pub fn collection_counters(&self) -> (u64, u64, u64) {
        (
            self.gc_stats.gc_cycles,
            self.gc_stats.minor_gc_cycles,
            self.gc_stats.minor_slots_scanned,
        )
    }

    /// Per-heap GC counters and the per-type breakdown. The runtime
    /// allocator updates its type's row; the aggregates `live_objects` and
    /// `live_bytes` are summed from the rows here. `alloc_bytes_total` adds
    /// what generated code carved, derived from the allocation buffers'
    /// progress rather than counted per cell. A sweep still in progress
    /// completes first.
    pub fn gc_stats(&mut self) -> &GcStats {
        // A full collection's statistics are its completed sweep's.
        self.complete_old_sweep();
        let mut live_objects = 0usize;
        let mut live_bytes = 0usize;
        let mut alloc_bytes_total = 0u64;
        for row in &self.gc_stats.by_type {
            live_objects = live_objects.wrapping_add(row.live_count as usize);
            live_bytes = live_bytes.wrapping_add(row.live_bytes);
            alloc_bytes_total = alloc_bytes_total.saturating_add(row.alloc_bytes_total);
        }
        self.gc_stats.live_objects = live_objects;
        self.gc_stats.live_bytes = live_bytes;
        self.gc_stats.alloc_bytes_total = alloc_bytes_total.saturating_add(self.generated_bytes());
        &self.gc_stats
    }

    /// Bytes generated code has carved from allocation buffers: everything
    /// the buffers handed out, including the live one's progress, less what
    /// the runtime allocator bumped from them.
    pub(super) fn generated_bytes(&self) -> u64 {
        let live = if self.lab_page == 0 {
            0
        } else {
            (self.lab.top - self.lab_start) as u64
        };
        (self.lab_consumed_bytes + live).saturating_sub(self.runtime_lab_bytes)
    }

    /// Book the bytes a restored image brought in against the heap cap and
    /// the growth trigger, and its bodies (`(count, bytes)` per type tag) in
    /// the statistics, as ordinary old-space allocations would have.
    pub(crate) fn account_restored_image(&mut self, live_bytes: u64, by_type: &[(usize, usize)]) {
        self.tracked_bytes = self.tracked_bytes.saturating_add(live_bytes);
        for (tag, &(count, bytes)) in by_type.iter().enumerate() {
            if count == 0 {
                continue;
            }
            self.gc_stats.live_objects = self.gc_stats.live_objects.wrapping_add(count);
            self.gc_stats.live_bytes = self.gc_stats.live_bytes.wrapping_add(bytes);
            let row = &mut self.gc_stats.by_type[tag];
            row.live_bytes = row.live_bytes.wrapping_add(bytes);
            row.live_count = row.live_count.wrapping_add(count as u64);
            self.rows_live_bytes = self.rows_live_bytes.wrapping_add(bytes);
        }
    }
}
