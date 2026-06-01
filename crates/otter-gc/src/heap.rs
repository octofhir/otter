//! Top-level GC heap — owns the spaces, the handle stacks, the
//! marking state, and the trace table.
//!
//! # Contents
//!
//! - [`GcHeap`] — the orchestrator the rest of the runtime sees.
//! - [`RootSlotVisitor`] — caller-supplied root source closure for full GC.
//! - [`HeapStats`] — tiny snapshot of accounting (used by tests).
//! - Ephemeron registry and split mark/sweep hooks used by weak
//!   collections in the VM.
//! - Weak-reference/finalization registry bookkeeping used by VM
//!   post-mark processing.
//!
//! # Invariants
//!
//! - Every public `alloc<T>` records `T::TYPE_TAG` on the
//!   [`crate::header::GcHeader`]; full GC dispatches through the
//!   trace table by tag.
//! - Black allocation: when a marking cycle is in progress
//!   (`marking.is_marking() == true`), new objects start black so
//!   the marker doesn't have to re-discover them. Phase-1 STW
//!   marker never observes the flag set during the mutator phase
//!   so the fast path costs one branch.
//! - Pages live forever inside the heap or are returned to the
//!   cage on full-GC sweep. Pages are never leaked across heap
//!   drops.
//! - Weak collection tables are registered as type-erased raw
//!   handles; VM code runs the ephemeron fixpoint between
//!   [`GcHeap::mark_phase`] and [`GcHeap::sweep_phase`].
//!
//! # See also
//!
//! - GC architecture plan §6.1 (unsafe boundary).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::compressed::{Cage, Gc, RawGc, cage_base};
use crate::ephemeron::EphemeronRegistry;
use crate::external::{ExternalMemory, SharedExternalMemory, SharedExternalState};
use crate::extra_roots::ExtraRoots;
use crate::finalize::WeakFinalizationRegistry;
use crate::frame_roots::{FrameRootProviders, FrameRoots};
use crate::handle::{GlobalHandleTable, HandleStack};
use crate::header::{GcHeader, MarkColor};
use crate::marking::MarkingState;
use crate::oom::OutOfMemory;
use crate::page::{CELL_SIZE, align_up};
use crate::page::{LARGE_OBJECT_THRESHOLD, PAGE_HEADER_SIZE, page_base_from_offset};
use crate::scavenger::ScavengeStats;
use crate::space::{LargeObjectSpace, NewSpace, OldSpace};
use crate::stats::{GcStats, TYPE_TAG_COUNT};
use crate::store::GcStore;
use crate::trace::{TraceTable, Traceable};

/// Type alias for the higher-order visitor closure used by the
/// GC: it receives a `&mut dyn FnMut(*mut RawGc)` slot visitor
/// and emits one slot pointer per outgoing reference. Hoisted out
/// of the public method signatures to keep clippy's
/// `type_complexity` lint quiet.
pub type RootSlotVisitor<'a> = dyn FnMut(&mut dyn FnMut(*mut RawGc)) + 'a;

/// Empty root set — every reachable object must come from a
/// handle scope or a global handle.
pub fn empty() -> EmptyRoots {
    EmptyRoots
}

/// Trivial empty-roots implementation; useful in tests where all
/// reachable objects sit in handle scopes.
pub struct EmptyRoots;

/// Lightweight stats snapshot.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeapStats {
    /// Total bytes allocated across all spaces.
    pub allocated_bytes: usize,
    /// Pages owned by all spaces.
    pub page_count: usize,
    /// New-space allocated bytes.
    pub new_allocated_bytes: usize,
    /// Old-space allocated bytes.
    pub old_allocated_bytes: usize,
    /// Most recent scavenge result.
    pub last_scavenge: ScavengeStats,
    /// Bytes reclaimed by the most recent full GC.
    pub last_full_reclaimed: usize,
    /// Bytes currently tracked against the configured cap (sum
    /// of slot allocations + outstanding [`GcHeap::reserve_bytes`]
    /// reservations). `0` until the first allocation.
    pub tracked_bytes: u64,
    /// Outstanding off-slot/external bytes reserved through
    /// [`GcHeap::reserve_bytes`] or [`GcHeap::reserve_external`].
    pub reserved_bytes: u64,
    /// Configured heap cap in bytes (`0` = disabled).
    pub max_heap_bytes: u64,
}

/// Orchestrator. Owned by the runtime; passed by `&mut` to every
/// allocation / barrier / GC call.
///
/// Field order is **load-bearing for hot-path performance**:
/// `max_heap_bytes` and `tracked_bytes` lead so the alloc cap
/// check shares a cache line with `new_space`'s bump cursor,
/// avoiding a second cache miss in the steady-state allocation
/// loop.
pub struct GcHeap {
    /// Configured per-heap soft cap (`0` = disabled). Front-of-
    /// struct so the alloc fast-path's "is the cap enabled?"
    /// check shares a cache line with [`Self::new_space`].
    max_heap_bytes: u64,
    /// Bytes currently tracked against [`Self::max_heap_bytes`].
    /// Sum of slot allocations + [`Self::reserve_bytes`]
    /// reservations, recomputed from the live spaces after every
    /// emergency full GC. Always `0` for an empty heap.
    tracked_bytes: u64,
    /// Outstanding [`Self::reserve_bytes`] reservations. Tracked
    /// separately from slot allocations so emergency-GC
    /// reconciliation does not lose them.
    reserved_bytes: u64,
    new_space: NewSpace,
    old_space: OldSpace,
    large_space: LargeObjectSpace,
    trace_table: TraceTable,
    marking: MarkingState,
    handle_stack: Box<HandleStack>,
    global_handles: Box<GlobalHandleTable>,
    extra_roots: Option<ExtraRoots>,
    frame_root_providers: FrameRootProviders,
    ephemerons: EphemeronRegistry,
    weak_finalization: WeakFinalizationRegistry,
    shared_external: Arc<SharedExternalState>,
    stats: HeapStats,
    gc_stats: GcStats,
    /// Cooperative-cancellation flag; flipped to `true` when the
    /// cap rejects an allocation. Watchdogs may poll this between
    /// safepoints to short-circuit, but the **primary** OOM signal
    /// is the `Err(OutOfMemory)` returned from [`Self::alloc`] —
    /// the alloc is **never** materialised on a cap miss
    /// (architecture plan §2.1 caveat).
    oom_flag: Arc<AtomicBool>,
}

impl GcHeap {
    /// Build a fresh heap with no cap. Equivalent to
    /// `Self::with_max_heap_bytes(0)`.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfMemory`] if the cage cannot be initialised
    /// (cage exhausted or alloc failed).
    pub fn new() -> Result<Self, OutOfMemory> {
        Self::with_max_heap_bytes(0)
    }

    /// Build a fresh heap honouring a per-heap byte cap.
    ///
    /// `cap == 0` disables the cap; allocations succeed until the
    /// cage is exhausted. `cap > 0` is **load-bearing**: an
    /// allocation that would overshoot the cap triggers one
    /// emergency full GC and, if the cap is still exceeded, is
    /// refused with [`OutOfMemory::HeapCapExceeded`]. The slot is
    /// never materialised (architecture plan §2.1 caveat).
    ///
    /// # Errors
    ///
    /// Returns [`OutOfMemory`] when the cage cannot satisfy the
    /// initial new-space.
    pub fn with_max_heap_bytes(cap: u64) -> Result<Self, OutOfMemory> {
        Cage::ensure_default().map_err(|_| OutOfMemory::CageExhausted)?;
        let new_space = NewSpace::new(crate::space::DEFAULT_NEW_SPACE_PAGES)?;
        Ok(Self {
            new_space,
            old_space: OldSpace::new(),
            large_space: LargeObjectSpace::new(),
            trace_table: TraceTable::new(),
            marking: MarkingState::new(),
            handle_stack: Box::new(HandleStack::new()),
            global_handles: Box::new(GlobalHandleTable::new()),
            extra_roots: None,
            frame_root_providers: FrameRootProviders::new(),
            ephemerons: EphemeronRegistry::default(),
            weak_finalization: WeakFinalizationRegistry::default(),
            shared_external: Arc::new(SharedExternalState::default()),
            stats: HeapStats::default(),
            gc_stats: GcStats::default(),
            max_heap_bytes: cap,
            tracked_bytes: 0,
            reserved_bytes: 0,
            oom_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Register an active interpreter frame-stack root provider.
    ///
    /// Returns the new provider depth; callers pass `depth - 1` to
    /// [`Self::pop_frame_roots_to`] when leaving the matching dispatch scope.
    pub fn push_frame_roots(&mut self, provider: *const dyn FrameRoots) -> usize {
        self.frame_root_providers.push(provider)
    }

    /// Truncate active frame-root providers back to `depth`.
    pub fn pop_frame_roots_to(&mut self, depth: usize) {
        self.frame_root_providers.pop_to(depth);
    }

    /// Visit every active interpreter frame-stack root provider.
    pub fn trace_frame_root_providers(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.frame_root_providers.trace(visitor);
    }

    /// `true` when an active interpreter frame stack is registered.
    #[must_use]
    pub fn has_frame_root_providers(&self) -> bool {
        !self.frame_root_providers.is_empty()
    }

    /// Configured per-heap cap in bytes (`0` = disabled).
    pub fn max_heap_bytes(&self) -> u64 {
        self.max_heap_bytes
    }

    /// Bytes currently tracked against the cap (slot allocations
    /// + outstanding [`Self::reserve_bytes`] reservations).
    pub fn tracked_bytes(&self) -> u64 {
        self.effective_tracked_bytes()
    }

    fn pending_shared_external_releases(&self) -> u64 {
        self.shared_external
            .released_bytes()
            .min(self.reserved_bytes)
    }

    fn effective_reserved_bytes(&self) -> u64 {
        self.reserved_bytes
            .saturating_sub(self.pending_shared_external_releases())
    }

    fn effective_tracked_bytes(&self) -> u64 {
        if self.max_heap_bytes == 0 {
            return self.tracked_bytes;
        }
        self.tracked_bytes
            .saturating_sub(self.pending_shared_external_releases())
    }

    fn drain_shared_external_releases(&mut self) {
        let released = self.shared_external.take_released_bytes();
        if released == 0 {
            return;
        }
        let actual = released.min(self.reserved_bytes);
        self.reserved_bytes = self.reserved_bytes.saturating_sub(actual);
        if self.max_heap_bytes != 0 {
            self.tracked_bytes = self.tracked_bytes.saturating_sub(actual);
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
    /// On overshoot: one emergency full GC, then retry once. If
    /// still over → [`OutOfMemory::HeapCapExceeded`] and the
    /// reservation is **not** booked.
    ///
    /// # Errors
    ///
    /// [`OutOfMemory::HeapCapExceeded`] when the cap is enabled
    /// and the reservation cannot be satisfied even after an
    /// emergency full GC.
    pub fn reserve_bytes(&mut self, bytes: u64) -> Result<(), OutOfMemory> {
        self.drain_shared_external_releases();
        if self.max_heap_bytes == 0 {
            self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
            return Ok(());
        }
        if bytes > self.max_heap_bytes {
            self.oom_flag.store(true, Ordering::Relaxed);
            return Err(OutOfMemory::HeapCapExceeded {
                requested_bytes: bytes,
                heap_limit_bytes: self.max_heap_bytes,
            });
        }
        self.account_or_collect(bytes)?;
        self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
        Ok(())
    }

    /// Reserve `bytes` against the cap while exposing caller-owned roots to a
    /// possible emergency full GC.
    ///
    /// Use this for off-slot backing storage that is being prepared by a
    /// mutator stack frame whose values are not all present in heap
    /// handle/global tables. The reservation is booked only after the cap
    /// check succeeds.
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
        if self.max_heap_bytes == 0 {
            self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
            return Ok(());
        }
        if bytes > self.max_heap_bytes {
            self.oom_flag.store(true, Ordering::Relaxed);
            return Err(OutOfMemory::HeapCapExceeded {
                requested_bytes: bytes,
                heap_limit_bytes: self.max_heap_bytes,
            });
        }
        self.account_or_collect_with_roots(bytes, external_visit)?;
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
    /// VM containers use this for capacity growth while their
    /// roots live in interpreter frames. A heap-local emergency GC
    /// cannot see those frame roots, so the only sound cap behavior
    /// at this boundary is to refuse the reservation and let the VM
    /// synthesize a catchable diagnostic.
    ///
    /// # Errors
    ///
    /// [`OutOfMemory::HeapCapExceeded`] when `bytes` would exceed
    /// the configured cap.
    pub fn reserve_bytes_no_collect(&mut self, bytes: u64) -> Result<(), OutOfMemory> {
        self.drain_shared_external_releases();
        if self.max_heap_bytes == 0 {
            self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
            return Ok(());
        }
        let projected = self.tracked_bytes.saturating_add(bytes);
        if projected > self.max_heap_bytes {
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
    }

    /// Account `bytes` against the cap. Outlined so the alloc
    /// hot path adds only a single cap-enabled branch when the
    /// cap is disabled. On overshoot: one emergency full GC,
    /// retry once, otherwise refuse.
    ///
    /// Caller pre-checks `self.max_heap_bytes != 0`; this
    /// function is not invoked when the cap is disabled, so the
    /// disabled-cap branch never executes here.
    #[cold]
    #[inline(never)]
    fn account_or_collect_with_roots(
        &mut self,
        bytes: u64,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.drain_shared_external_releases();
        let cap = self.max_heap_bytes;
        let projected = self.tracked_bytes.saturating_add(bytes);
        if projected <= cap {
            self.tracked_bytes = projected;
            return Ok(());
        }
        self.collect_full(external_visit);
        self.tracked_bytes = self.live_bytes_total().saturating_add(self.reserved_bytes);
        let projected = self.tracked_bytes.saturating_add(bytes);
        if projected <= cap {
            self.tracked_bytes = projected;
            return Ok(());
        }
        self.oom_flag.store(true, Ordering::Relaxed);
        Err(OutOfMemory::HeapCapExceeded {
            requested_bytes: bytes,
            heap_limit_bytes: cap,
        })
    }

    #[cold]
    #[inline(never)]
    fn account_or_collect(&mut self, bytes: u64) -> Result<(), OutOfMemory> {
        // External root visitor is empty — the handle stack and
        // global handle table are walked from inside
        // `collect_full`, and the embedder has no opportunity to
        // re-enter the heap during a refused allocation.
        let mut noop = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.account_or_collect_with_roots(bytes, &mut noop)
    }

    /// Sum of allocated bytes across new + old + LOS. Counts
    /// post-collect retained-page slack as well as live objects;
    /// strict liveness arrives with task 86's incremental sweep.
    fn live_bytes_total(&self) -> u64 {
        let new = self.new_space.allocated_bytes() as u64;
        let old = self.old_space.allocated_bytes() as u64;
        let los = self.large_space.allocated_bytes() as u64;
        new.saturating_add(old).saturating_add(los)
    }

    /// Register a [`Traceable`] type so allocations of `T` can be
    /// traced during marking and scavenging.
    pub fn register_traceable<T: Traceable>(&mut self) {
        self.trace_table.register::<T>();
    }

    /// Register a [`SafeFinalize`] body so the sweeper calls
    /// `finalize_safe` on dead instances before their storage is
    /// reclaimed. Idempotent; must be paired with an earlier
    /// `register_traceable::<T>()` (the lazy `alloc_*` path
    /// handles the trace registration for you).
    pub fn register_finalize<T: Traceable + crate::trace::SafeFinalize>(&mut self) {
        self.trace_table.register_finalize::<T>();
    }

    /// Register a GC-managed weak collection table for ephemeron
    /// fixpoint work between full-GC marking and sweeping.
    pub fn register_ephemeron_table<T: ?Sized>(&mut self, table: Gc<T>) {
        self.ephemerons.register(table.raw());
    }

    /// Register a GC-managed `WeakRef` body for post-mark clearing.
    pub fn register_weak_ref<T: ?Sized>(&mut self, weak_ref: Gc<T>) {
        self.weak_finalization.register_weak_ref(weak_ref.raw());
    }

    /// Register a GC-managed `FinalizationRegistry` body for
    /// post-mark cleanup enqueueing.
    pub fn register_finalization_registry<T: ?Sized>(&mut self, registry: Gc<T>) {
        self.weak_finalization
            .register_finalization_registry(registry.raw());
    }

    /// Snapshot registered weak collection tables.
    #[doc(hidden)]
    #[must_use]
    pub fn ephemeron_tables_snapshot(&self) -> Vec<RawGc> {
        self.ephemerons.snapshot()
    }

    /// Number of registered weak collection tables.
    #[must_use]
    pub fn ephemeron_table_count(&self) -> usize {
        self.ephemerons.len()
    }

    /// Snapshot registered `WeakRef` body handles.
    #[doc(hidden)]
    #[must_use]
    pub fn weak_refs_snapshot(&self) -> Vec<RawGc> {
        self.weak_finalization.weak_refs_snapshot()
    }

    /// Snapshot registered `FinalizationRegistry` body handles.
    #[doc(hidden)]
    #[must_use]
    pub fn finalization_registries_snapshot(&self) -> Vec<RawGc> {
        self.weak_finalization.finalization_registries_snapshot()
    }

    /// Whether this heap has ever allocated a finalization registry.
    #[must_use]
    pub fn has_finalization_registries(&self) -> bool {
        self.weak_finalization.has_finalization_registries()
    }

    /// Whether there is no registered weak-reference/finalization work.
    #[must_use]
    pub fn weak_finalization_registry_is_empty(&self) -> bool {
        self.weak_finalization.is_empty()
    }

    /// Count registered `WeakRef` bodies.
    #[must_use]
    pub fn weak_ref_count(&self) -> usize {
        self.weak_finalization.weak_ref_count()
    }

    /// Count registered `FinalizationRegistry` bodies.
    #[must_use]
    pub fn finalization_registry_count(&self) -> usize {
        self.weak_finalization.finalization_registry_count()
    }

    /// Reference to the trace table (used by tests).
    #[doc(hidden)]
    pub fn trace_table(&self) -> &TraceTable {
        &self.trace_table
    }

    /// Borrow the heap's handle stack.
    pub fn handle_stack(&self) -> &HandleStack {
        &self.handle_stack
    }

    /// Raw pointer to the heap's handle stack. Used by
    /// [`HandleScope::from_ptr`] to open a scope without
    /// holding an immutable borrow on the heap — the heap can
    /// continue to be mutated (`alloc`, `collect_*`,
    /// `write_barrier`) while the scope is open. The Box-owned
    /// stack has a stable address that lives as long as the
    /// `GcHeap`.
    pub fn handle_stack_ptr(&self) -> *const HandleStack {
        &*self.handle_stack as *const _
    }

    /// Borrow the heap's global handle table for crate-internal
    /// rooting APIs.
    pub(crate) fn global_handles(&self) -> &GlobalHandleTable {
        &self.global_handles
    }

    /// Install or clear the heap's extra runtime root source, returning the
    /// previous registration so callers can restore nested scopes.
    #[must_use]
    pub fn install_extra_roots(&mut self, roots: Option<ExtraRoots>) -> Option<ExtraRoots> {
        std::mem::replace(&mut self.extra_roots, roots)
    }

    /// Reference to the marking state (Phase 2 / task 86 will
    /// drive it).
    pub fn marking(&self) -> &MarkingState {
        &self.marking
    }

    /// Mutable marking state (Phase 2).
    pub fn marking_mut(&mut self) -> &mut MarkingState {
        &mut self.marking
    }

    /// Stats snapshot.
    pub fn stats(&self) -> HeapStats {
        let mut s = self.stats;
        s.new_allocated_bytes = self.new_space.allocated_bytes();
        s.old_allocated_bytes = self.old_space.allocated_bytes();
        s.allocated_bytes =
            s.new_allocated_bytes + s.old_allocated_bytes + self.large_space.allocated_bytes();
        s.page_count = self.new_space.from_page_count() * 2
            + self.old_space.page_count()
            + self.large_space.page_count();
        s.tracked_bytes = self.effective_tracked_bytes();
        s.reserved_bytes = self.effective_reserved_bytes();
        s.max_heap_bytes = self.max_heap_bytes;
        s
    }

    /// Allocate a `T` on the GC heap.
    ///
    /// The header gets `T::TYPE_TAG`; the value is moved into the
    /// payload area. When marking is active the new object starts
    /// black (V8 black-allocation since 2018).
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
        mut value: T,
        external_visit: &mut RootSlotVisitor<'_>,
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
        let total = std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>();
        let aligned = align_up(total, CELL_SIZE);
        debug_assert!(
            aligned <= u32::MAX as usize,
            "object size exceeds u32 limit"
        );

        // Cap check — runs before any slot is carved (architecture
        // §2.1 caveat). When the cap is disabled (`max_heap_bytes
        // == 0`), the hot path adds one load + branch and skips
        // accounting entirely; the cap-enabled path is outlined
        // under [`Self::account_or_collect_with_roots`].
        let pending_value = std::ptr::addr_of_mut!(value);
        let mut allocation_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            // SAFETY: `pending_value` points at the still-unpublished
            // allocation payload on this stack frame. During the STW
            // collection triggered by this allocation, tracing may rewrite any
            // GC slots embedded in that payload before it is copied into the
            // freshly carved heap cell below.
            unsafe {
                T::trace_slots(pending_value, visitor);
            }
        };

        if self.max_heap_bytes != 0 {
            self.account_or_collect_with_roots(aligned as u64, &mut allocation_roots)?;
        }

        let is_marking = self.marking.is_marking();

        let offset = if aligned > LARGE_OBJECT_THRESHOLD {
            self.large_space.alloc(aligned)?
        } else {
            // Try young-gen first; if full, scavenge then retry.
            match self.new_space.alloc(aligned) {
                Some(off) => off,
                None => {
                    // Trigger scavenge with caller-supplied
                    // external roots; handle stack + globals are
                    // walked internally.
                    self.collect_minor_internal(&mut allocation_roots);
                    self.new_space
                        .alloc(aligned)
                        .ok_or(OutOfMemory::CageExhausted)?
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
            let header = if aligned > LARGE_OBJECT_THRESHOLD {
                let h = GcHeader::new(T::TYPE_TAG, aligned as u32);
                if is_marking {
                    h.set_mark_color(MarkColor::Black);
                }
                h
            } else if is_marking {
                GcHeader::new_young_black(T::TYPE_TAG, aligned as u32)
            } else {
                GcHeader::new_young(T::TYPE_TAG, aligned as u32)
            };
            std::ptr::write(header_ptr, header);
            std::ptr::write(payload_ptr, value);
        }
        // Counter wiring inlined to keep the alloc fast path
        // tight. Only the per-tag row is touched (one cache
        // line, fixed offset since `T::TYPE_TAG` is const); the
        // aggregate `live_objects` / `live_bytes` are derived in
        // [`Self::gc_stats`] / [`Self::reconcile_live_counts`].
        // Wrapping arithmetic — overflow is unreachable in
        // practice (≥ 1.8 × 10¹⁹ allocs).
        let row = &mut self.gc_stats.by_type[T::TYPE_TAG as usize];
        row.live_bytes = row.live_bytes.wrapping_add(aligned);
        row.alloc_count_total = row.alloc_count_total.wrapping_add(1);
        row.alloc_bytes_total = row
            .alloc_bytes_total
            .wrapping_add(u64::try_from(aligned).unwrap_or(u64::MAX));
        // SAFETY: offset is the cage offset of a freshly-alloc-ed
        // T payload; type tag matches `T::TYPE_TAG`.
        Ok(unsafe { Gc::from_offset(offset) })
    }

    /// Allocate a `T` directly in old-space, bypassing
    /// young-gen entirely.
    ///
    /// Phase-1 escape hatch for callers that hold long-lived
    /// references through `Rc<…>` containers the GC can't yet
    /// rewrite (e.g. `Rc<[Gc<T>]>` upvalue spines from task 76).
    /// Old-space objects do not move, so a slot stored in an
    /// `Rc`-shared container stays valid across collections.
    /// Migrations that wire frame-stack tracing (Phase 2,
    /// task 86) may switch back to [`Self::alloc`] once the
    /// scavenger can fix up every container slot.
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
        self.alloc_old_with_roots_inner(value, true, &mut empty)
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
        self.alloc_old_with_roots_inner(value, true, external_visit)
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
        let mut empty = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let out = self.alloc_old_with_roots_inner(value, false, &mut empty)?;
        if self.max_heap_bytes != 0 {
            self.drain_shared_external_releases();
            self.tracked_bytes = self.live_bytes_total().saturating_add(self.reserved_bytes);
            self.oom_flag.store(true, Ordering::Relaxed);
        }
        Ok(out)
    }

    #[inline]
    fn alloc_old_with_roots_inner<T: Traceable>(
        &mut self,
        mut value: T,
        enforce_cap: bool,
        external_visit: &mut RootSlotVisitor<'_>,
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
        let total = std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>();
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
            unsafe {
                T::trace_slots(pending_value, visitor);
            }
        };
        if enforce_cap && self.max_heap_bytes != 0 {
            self.account_or_collect_with_roots(aligned as u64, &mut allocation_roots)?;
        }
        let is_marking = self.marking.is_marking();
        let offset = if aligned > LARGE_OBJECT_THRESHOLD {
            self.large_space.alloc(aligned)?
        } else {
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
            let header = if is_marking {
                let h = GcHeader::new(T::TYPE_TAG, aligned as u32);
                h.set_mark_color(MarkColor::Black);
                h
            } else {
                GcHeader::new(T::TYPE_TAG, aligned as u32)
            };
            std::ptr::write(header_ptr, header);
            std::ptr::write(payload_ptr, value);
        }
        let row = &mut self.gc_stats.by_type[T::TYPE_TAG as usize];
        row.live_bytes = row.live_bytes.wrapping_add(aligned);
        row.alloc_count_total = row.alloc_count_total.wrapping_add(1);
        row.alloc_bytes_total = row
            .alloc_bytes_total
            .wrapping_add(u64::try_from(aligned).unwrap_or(u64::MAX));
        // SAFETY: offset references a fresh `T` payload.
        Ok(unsafe { Gc::from_offset(offset) })
    }

    /// Read-only access to the payload of a heap-allocated
    /// value. The closure receives `&T`; never holds the
    /// reference past return.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `handle.is_null()`.
    #[inline]
    pub fn read_payload<T: Traceable, R>(&self, handle: Gc<T>, f: impl FnOnce(&T) -> R) -> R {
        debug_assert!(!handle.is_null(), "read_payload on null handle");
        // SAFETY: handle was issued by `alloc` / `alloc_old`;
        // payload lives immediately after the header at the
        // same allocation; single-mutator GC (no concurrent
        // mutator).
        unsafe {
            let payload = (handle.as_header_ptr() as *mut u8).add(std::mem::size_of::<GcHeader>())
                as *const T;
            f(&*payload)
        }
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
        // SAFETY: same contract as [`Self::read_payload`]; the
        // exclusive `&mut self` upholds payload uniqueness.
        unsafe {
            let payload =
                (handle.as_header_ptr() as *mut u8).add(std::mem::size_of::<GcHeader>()) as *mut T;
            f(&mut *payload)
        }
    }

    /// Run a minor GC (Cheney scavenge).
    pub fn collect_minor(&mut self, _roots: EmptyRoots) {
        let empty: fn(&mut dyn FnMut(*mut RawGc)) = |_| {};
        self.collect_minor_internal(&mut |v| empty(v));
    }

    /// Run a minor GC with caller-supplied root visitor.
    pub fn collect_minor_with_roots(&mut self, external_visit: &mut RootSlotVisitor<'_>) {
        self.collect_minor_internal(external_visit);
    }

    fn collect_minor_internal(&mut self, external_visit: &mut RootSlotVisitor<'_>) {
        // Combine the caller's external_visit with the heap's
        // own handle-stack and global-handles walk.
        let handle_stack: *const HandleStack = &*self.handle_stack;
        let global_handles: *const GlobalHandleTable = &*self.global_handles;
        let extra_roots = self.extra_roots;
        let frame_root_providers: *const FrameRootProviders = &self.frame_root_providers;
        let mut combined = move |visitor: &mut dyn FnMut(*mut RawGc)| {
            // SAFETY: STW pause; raw pointers reconstituted to
            // shared references.
            unsafe {
                (*handle_stack).visit_slots(visitor);
                (*global_handles).visit_slots(visitor);
            }
            external_visit(visitor);
            if let Some(extra) = extra_roots {
                extra.visit(visitor);
            }
            // SAFETY: STW pause; the heap owns the registry for the duration of
            // this root walk.
            unsafe { (*frame_root_providers).trace(visitor) };
        };
        let ephemeron_registry_slots = self.ephemerons.handle_slots();
        let weak_registry_slots = self.weak_finalization.handle_slots();
        // SAFETY: STW pause for the duration of the call;
        // every type tag in from-space is registered.
        let stats = unsafe {
            crate::scavenger::scavenge(
                &mut self.new_space,
                &mut self.old_space,
                &self.trace_table,
                &[],
                &mut combined,
                &ephemeron_registry_slots,
                &weak_registry_slots,
            )
        };
        self.ephemerons.retain_non_null();
        self.weak_finalization.retain_non_null();
        self.stats.last_scavenge = stats;
        // Per-tag counters drift between scavenges (young
        // allocations counted at alloc-time but never
        // decremented when the scavenger reclaims them); the
        // drift is corrected on the next full GC via
        // [`Self::reconcile_live_counts`]. Scavenge keeps the
        // hot path tight by not walking the live set just for
        // stats — see `bench_scavenge_4mb` for the cost of
        // adding a walk here.
    }

    /// Run a full GC (young-gen scavenge + old-gen mark-sweep).
    ///
    /// `external_visit` yields every external root slot.
    pub fn collect_full(&mut self, external_visit: &mut RootSlotVisitor<'_>) {
        let pause_start = Instant::now();
        self.mark_phase(external_visit);
        self.sweep_phase_with_pause_start(pause_start);
    }

    /// Begin a full-GC mark phase and drain the ordinary root graph.
    ///
    /// Ephemeron users may call [`Self::mark_additional`] after this
    /// method and before [`Self::sweep_phase`] to run a weak-table
    /// fixpoint.
    ///
    /// This convenience wrapper performs the entire mark phase under
    /// one STW pause; the incremental driver
    /// ([`Self::start_incremental_mark_phase`] /
    /// [`Self::incremental_mark_step`] /
    /// [`Self::finish_incremental_mark_phase`]) splits the same work
    /// across multiple safepoints so the mutator can run between
    /// drain steps. Both paths leave the heap in identical states.
    pub fn mark_phase(&mut self, external_visit: &mut RootSlotVisitor<'_>) {
        self.start_incremental_mark_phase(external_visit);
        // SAFETY: STW pause; all pushed headers alive.
        unsafe {
            self.marking.drain_full(&self.trace_table);
        }
    }

    /// Begin an incremental old-gen mark cycle.
    ///
    /// Performs the parts of [`Self::mark_phase`] that can only run
    /// under an STW pause — pre-scavenge, mark-bit reset, root scan
    /// — then returns with `is_marking == true`. The mutator may
    /// resume between this call and the matching
    /// [`Self::finish_incremental_mark_phase`]:
    ///
    /// - Pointer stores route through [`Self::write_barrier`] /
    ///   [`Self::record_write`]; the **insertion** half of the
    ///   barrier shades any white child the mutator publishes
    ///   (Dijkstra invariant).
    /// - New old-gen allocations are stamped black at birth so the
    ///   mark cycle never traces freshly-published children.
    ///
    /// Drain progress is made by repeatedly calling
    /// [`Self::incremental_mark_step`]. Once the mutator is ready
    /// to finish, [`Self::finish_incremental_mark_phase`] re-scans
    /// roots (catching any updates the barrier could not observe —
    /// e.g. handles popped & re-pushed mid-cycle) and drains to
    /// completion.
    ///
    /// # See also
    /// - V8 incremental marker — Dijkstra-style insertion barrier
    ///   with black-on-allocation new-object policy
    ///   (<https://v8.dev/blog/concurrent-marking>).
    pub fn start_incremental_mark_phase(&mut self, external_visit: &mut RootSlotVisitor<'_>) {
        // Scavenge first so survivors are in old / to-space.
        self.collect_minor_internal(external_visit);

        // Reset old-space + LOS live counters; mark cycle.
        self.old_space.reset_live_bytes();
        self.large_space.reset_live_bytes();
        // Reset every header's mark to white before we begin.
        // SAFETY: STW pause + we own the spaces.
        unsafe {
            for page in self.old_space.pages() {
                page.for_each_object(|h, _| {
                    (*h).clear_mark();
                });
            }
            for page in self.large_space.pages() {
                page.for_each_object(|h, _| {
                    (*h).clear_mark();
                });
            }
            // Young-gen survivors after scavenge live in
            // to-space — but the scavenger has flipped, so they
            // now live in from-space. Clear marks there too.
            for page in self.new_space.from_pages() {
                page.for_each_object(|h, _| {
                    (*h).clear_mark();
                });
            }
        }

        self.marking.start_cycle();
        self.shade_roots(external_visit);
    }

    /// Drain at most `budget` gray headers from the marking
    /// worklist. Returns the number of headers processed (always
    /// `<= budget`); a return of `0` signals the worklist is
    /// currently empty, though the insertion barrier may push more
    /// before [`Self::finish_incremental_mark_phase`] is called.
    ///
    /// Calling this outside an active mark cycle is a no-op.
    #[must_use]
    pub fn incremental_mark_step(&mut self, budget: usize) -> usize {
        if !self.marking.is_marking() {
            return 0;
        }
        // SAFETY: every header on the worklist was pushed while
        // alive. New old-gen allocations during the cycle are
        // black-at-birth so they never enter the worklist; the
        // mutator cannot drop a live old-gen object out from under
        // a gray header (sweep is gated on the matching
        // `finish_incremental_mark_phase` returning).
        unsafe { self.marking.drain_with_budget(budget, &self.trace_table) }
    }

    /// Re-scan roots and drain the worklist to completion.
    ///
    /// Pairs with [`Self::start_incremental_mark_phase`]. After
    /// this returns the heap is in the same state as the end of
    /// [`Self::mark_phase`]: `is_marking == true` (cleared inside
    /// [`Self::sweep_phase`]), every reachable old-gen object is
    /// black, and the worklist is empty.
    pub fn finish_incremental_mark_phase(&mut self, external_visit: &mut RootSlotVisitor<'_>) {
        // Re-shade roots — the mutator may have rewritten handles
        // or globals between mark steps. The barrier covered every
        // *new* white-child publication, but a slot whose pointer
        // changed twice (new white → another white) only retains
        // its final value, so we walk the live root set again to
        // pick up any white target the barrier already shaded gray
        // (idempotent) plus any pointer the mutator stored straight
        // into a root slot (where no barrier fires).
        self.shade_roots(external_visit);
        // SAFETY: STW pause covers the final drain.
        unsafe {
            self.marking.drain_full(&self.trace_table);
        }
    }

    fn shade_roots(&mut self, external_visit: &mut RootSlotVisitor<'_>) {
        let handle_stack: *const HandleStack = &*self.handle_stack;
        let global_handles: *const GlobalHandleTable = &*self.global_handles;
        let marking_ptr = &mut self.marking as *mut MarkingState;
        let mut shade = move |slot: *mut RawGc| {
            // SAFETY: STW pause; valid slot per visitor contract.
            unsafe { (*marking_ptr).shade_from_slot(slot) };
        };
        // SAFETY: STW pause.
        unsafe {
            (*handle_stack).visit_slots(&mut shade);
            (*global_handles).visit_slots(&mut shade);
        }
        external_visit(&mut shade);
        if let Some(extra) = self.extra_roots {
            extra.visit(&mut shade);
        }
        self.frame_root_providers.trace(&mut shade);
    }

    /// Mark additional raw objects during an active mark phase and
    /// drain their transitive closure.
    ///
    /// Returns `true` when at least one previously-white object was
    /// discovered.
    #[doc(hidden)]
    pub fn mark_additional(&mut self, additions: impl IntoIterator<Item = RawGc>) -> bool {
        let mut discovered = false;
        for raw in additions {
            if raw.is_null() || self.is_marked(raw) {
                continue;
            }
            let mut slot = raw;
            // SAFETY: `raw` came from a live heap payload inspected
            // during the STW ephemeron phase.
            unsafe {
                self.marking.shade_from_slot(&mut slot as *mut RawGc);
            }
            discovered = true;
        }
        if discovered {
            // SAFETY: STW pause; all pushed headers are heap objects.
            unsafe {
                self.marking.drain_full(&self.trace_table);
            }
        }
        discovered
    }

    /// Returns true iff `raw` is marked in the current full-GC cycle.
    #[doc(hidden)]
    #[must_use]
    pub fn is_marked(&self, raw: RawGc) -> bool {
        if raw.is_null() {
            return false;
        }
        // SAFETY: caller supplies a heap-issued raw handle.
        unsafe { (*raw.as_header_ptr()).is_marked() }
    }

    /// Type tag for a raw heap object.
    #[doc(hidden)]
    #[must_use]
    pub fn raw_type_tag(&self, raw: RawGc) -> Option<u8> {
        if raw.is_null() {
            return None;
        }
        // SAFETY: caller supplies a heap-issued raw handle.
        Some(unsafe { (*raw.as_header_ptr()).type_tag() })
    }

    /// Type-check and cast a raw heap object to `Gc<T>`.
    #[doc(hidden)]
    #[must_use]
    pub fn cast_raw_if_type<T: Traceable>(&self, raw: RawGc) -> Option<Gc<T>> {
        if self.raw_type_tag(raw)? != T::TYPE_TAG {
            return None;
        }
        // SAFETY: the runtime type tag matches `T`.
        Some(unsafe { raw.cast::<T>() })
    }

    /// Prune the ephemeron registry to tables that survived the
    /// current mark phase. Must run before sweep frees dead tables.
    pub fn prune_ephemeron_registry_to_marked(&mut self) {
        let marked: std::collections::HashSet<RawGc> = self
            .ephemerons
            .snapshot()
            .into_iter()
            .filter(|raw| self.is_marked(*raw))
            .collect();
        self.ephemerons.retain_marked(|raw| marked.contains(&raw));
    }

    /// Prune weak-reference/finalization registries to handles
    /// that survived the current mark phase. Must run before sweep
    /// frees dead bodies.
    pub fn prune_weak_finalization_registry_to_marked(&mut self) {
        let marked: std::collections::HashSet<RawGc> = self
            .weak_finalization
            .weak_refs_snapshot()
            .into_iter()
            .chain(self.weak_finalization.finalization_registries_snapshot())
            .filter(|raw| self.is_marked(*raw))
            .collect();
        self.weak_finalization
            .retain_marked(|raw| marked.contains(&raw));
    }

    /// Finish a full-GC cycle by sweeping everything left white.
    pub fn sweep_phase(&mut self) {
        self.sweep_phase_with_pause_start(Instant::now());
    }

    fn sweep_phase_with_pause_start(&mut self, pause_start: Instant) {
        self.prune_ephemeron_registry_to_marked();
        self.prune_weak_finalization_registry_to_marked();

        // Sweep — anything still white in old / large / young
        // is dead. For old-space, walk pages; reap pages whose
        // live_bytes is zero. For young, our scavenger already
        // ran; any white survivor in from-space (post-flip) is
        // garbage we can drop at next scavenge but right now we
        // simply reset the whole thing — the next mutator alloc
        // path needs an empty from-space.
        //
        // The same walk also accumulates per-tag live counts
        // for [`crate::stats::GcStats`] reconciliation — fused
        // into one pass over the heap so the GC pause cost
        // stays bounded by the existing sweep work.
        // SAFETY: STW pause.
        let mut reclaimed = 0usize;
        let mut per_tag_live_count = [0u64; TYPE_TAG_COUNT];
        let mut per_tag_live_bytes = [0usize; TYPE_TAG_COUNT];
        unsafe {
            // Drop in-place + free per dead old-space object;
            // accumulate live counts in the same pass.
            for page in self.old_space.pages() {
                page.for_each_object(|h, _| {
                    let tag = (*h).type_tag() as usize;
                    let size = (*h).size_bytes() as usize;
                    if !(*h).is_marked() {
                        let tag_u8 = (*h).type_tag();
                        if let Some(finalize_fn) = self.trace_table.get_finalize(tag_u8) {
                            finalize_fn(h);
                        }
                        if let Some(drop_fn) = self.trace_table.get_drop(tag_u8) {
                            drop_fn(h);
                        }
                        reclaimed += size;
                    } else {
                        per_tag_live_count[tag] = per_tag_live_count[tag].wrapping_add(1);
                        per_tag_live_bytes[tag] = per_tag_live_bytes[tag].wrapping_add(size);
                    }
                });
            }
            for page in self.large_space.pages() {
                page.for_each_object(|h, _| {
                    let tag = (*h).type_tag() as usize;
                    let size = (*h).size_bytes() as usize;
                    if !(*h).is_marked() {
                        let tag_u8 = (*h).type_tag();
                        if let Some(finalize_fn) = self.trace_table.get_finalize(tag_u8) {
                            finalize_fn(h);
                        }
                        if let Some(drop_fn) = self.trace_table.get_drop(tag_u8) {
                            drop_fn(h);
                        }
                        reclaimed += size;
                    } else {
                        per_tag_live_count[tag] = per_tag_live_count[tag].wrapping_add(1);
                        per_tag_live_bytes[tag] = per_tag_live_bytes[tag].wrapping_add(size);
                    }
                });
            }
            // Young from-space is recycled at next scavenge but
            // any survivor still in to-space (after the
            // pre-sweep scavenge flip) belongs to the live set.
            // After the flip, those survivors live in
            // `from_pages()` of the new orientation.
            for page in self.new_space.from_pages() {
                page.for_each_object(|h, _| {
                    if (*h).is_marked() {
                        let tag = (*h).type_tag() as usize;
                        let size = (*h).size_bytes() as usize;
                        per_tag_live_count[tag] = per_tag_live_count[tag].wrapping_add(1);
                        per_tag_live_bytes[tag] = per_tag_live_bytes[tag].wrapping_add(size);
                    }
                });
            }
        }
        // Reap pages whose live bytes is zero.
        let _ = self.old_space.reap_dead_pages();
        let _ = self.large_space.reap_dead_pages();

        self.marking.finish_cycle();
        self.stats.last_full_reclaimed = reclaimed;
        self.gc_stats.last_gc_reclaimed_bytes = reclaimed;
        self.gc_stats.gc_cycles = self.gc_stats.gc_cycles.saturating_add(1);
        let elapsed = pause_start.elapsed();
        self.gc_stats.last_gc_pause_ms = elapsed.as_secs_f32() * 1000.0;
        // Commit the per-tag counters gathered during the sweep
        // pass — replaces the standalone `reconcile_live_counts`
        // walk so the full GC pays for at most one heap walk.
        for tag in 0..TYPE_TAG_COUNT {
            let row = &mut self.gc_stats.by_type[tag];
            row.live_bytes = per_tag_live_bytes[tag];
            row.free_count_total = row
                .alloc_count_total
                .saturating_sub(per_tag_live_count[tag]);
        }
    }

    /// Per-heap GC counters (alloc / live / free, per-type
    /// breakdown). The alloc fast path only updates the per-tag
    /// rows; the aggregates `live_objects` / `live_bytes` are
    /// derived from those rows here, so the returned snapshot
    /// is always consistent.
    pub fn gc_stats(&mut self) -> &GcStats {
        let mut live_objects = 0usize;
        let mut live_bytes = 0usize;
        let mut alloc_bytes_total = 0u64;
        for row in &self.gc_stats.by_type {
            // Live count per tag = alloc_count_total -
            // free_count_total; live_bytes is already maintained
            // per-tag by alloc + reconcile.
            let live_count = row.alloc_count_total.saturating_sub(row.free_count_total);
            live_objects = live_objects.wrapping_add(live_count as usize);
            live_bytes = live_bytes.wrapping_add(row.live_bytes);
            alloc_bytes_total = alloc_bytes_total.saturating_add(row.alloc_bytes_total);
        }
        self.gc_stats.live_objects = live_objects;
        self.gc_stats.live_bytes = live_bytes;
        self.gc_stats.alloc_bytes_total = alloc_bytes_total;
        &self.gc_stats
    }

    /// Single hot-path write barrier for a pointer-field store.
    ///
    /// Caller must:
    /// - have already performed the underlying store
    ///   `(*slot_addr) = child.raw()`,
    /// - pass the parent's `Gc<T>` (so the heap can locate the
    ///   parent's header),
    /// - pass the address of the slot inside the parent (so the
    ///   barrier can compute the card-bit position).
    pub fn write_barrier<T: ?Sized, U: ?Sized>(
        &mut self,
        parent: Gc<T>,
        slot_addr: *mut Gc<U>,
        child: Gc<U>,
    ) {
        if parent.is_null() {
            return;
        }
        let parent_header = parent.as_header_ptr();
        // SAFETY: parent is non-null; slot_addr is inside the
        // parent payload as required by the barrier contract.
        unsafe {
            crate::barrier::write_barrier(
                parent_header,
                slot_addr as *mut u8,
                child.raw(),
                &mut self.marking,
            );
        }
    }

    /// Record the outgoing edges established by storing `value` into
    /// `parent`.
    ///
    /// This is the safe contributor-facing mutation hook. Callers do
    /// not provide raw slot pointers and do not call barriers
    /// manually; they only pass the parent object and the value that
    /// was just stored. The heap computes a conservative slot address
    /// inside the parent payload for card-table marking.
    pub fn record_write<T: ?Sized, V: GcStore + ?Sized>(&mut self, parent: Gc<T>, value: &V) {
        if parent.is_null() {
            return;
        }
        let slot = parent_payload_slot(parent);
        let mut record = |edge: crate::GcEdge| {
            self.write_barrier_raw(parent, slot, edge.raw());
        };
        value.visit_gc_edges(&mut record);
    }

    /// Type-erased write barrier for callers that hold the
    /// child as a [`RawGc`] rather than a typed `Gc<U>`.
    /// Equivalent to [`Self::write_barrier`] otherwise.
    pub(crate) fn write_barrier_raw<T: ?Sized>(
        &mut self,
        parent: Gc<T>,
        slot_addr: *mut RawGc,
        child: RawGc,
    ) {
        if parent.is_null() {
            return;
        }
        let parent_header = parent.as_header_ptr();
        // SAFETY: parent is non-null; slot_addr is inside the
        // parent payload as required by the barrier contract.
        unsafe {
            crate::barrier::write_barrier(
                parent_header,
                slot_addr as *mut u8,
                child,
                &mut self.marking,
            );
        }
    }

    /// Iterate every live object in every space (debug / snapshot
    /// hook). Used by [`crate::devtools_snapshot`] and tests.
    ///
    /// # Safety
    ///
    /// Must run under STW pause; visitor must not allocate.
    pub unsafe fn for_each_live_object<F>(&self, mut visitor: F)
    where
        F: FnMut(*mut GcHeader),
    {
        // SAFETY: STW pause + valid pages.
        unsafe {
            for page in self.new_space.from_pages() {
                page.for_each_object(|h, _| visitor(h));
            }
            for page in self.old_space.pages() {
                page.for_each_object(|h, _| visitor(h));
            }
            for page in self.large_space.pages() {
                page.for_each_object(|h, _| visitor(h));
            }
        }
    }

    /// Trace the slots of a single object — exposed for the
    /// snapshot writer.
    ///
    /// # Safety
    ///
    /// `header` is a valid live header registered in the trace
    /// table.
    pub unsafe fn trace_one(&self, header: *mut GcHeader, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: per docstring.
        unsafe {
            self.trace_table.trace(header, visitor);
        }
    }

    /// Page-base lookup helper used by the snapshot.
    pub fn page_base_of(offset: u32) -> *mut u8 {
        page_base_from_offset(offset)
    }

    /// Page-header-size helper.
    pub fn page_header_size() -> usize {
        PAGE_HEADER_SIZE
    }
}

fn parent_payload_slot<T: ?Sized>(parent: Gc<T>) -> *mut RawGc {
    let body_base = parent.as_header_ptr() as *mut u8;
    body_base.wrapping_add(std::mem::size_of::<GcHeader>()) as *mut RawGc
}

impl std::fmt::Debug for GcHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcHeap")
            .field("new_pages", &self.new_space.from_page_count())
            .field("old_pages", &self.old_space.page_count())
            .field("large_pages", &self.large_space.page_count())
            .field("handles", &self.handle_stack.len())
            .field("is_marking", &self.marking.is_marking())
            .finish()
    }
}

// Drop: pages are owned by the spaces; their Drop returns them
// to the cage automatically.
