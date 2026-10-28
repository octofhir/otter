//! Top-level GC heap — owns the spaces, the handle stacks, the
//! marking state, and the trace table.
//!
//! # Contents
//!
//! - [`GcHeap`] — the orchestrator the rest of the runtime sees.
//! - [`RootSlotVisitor`] — caller-supplied root source closure for full GC.
//! - [`HeapStats`] — tiny snapshot of accounting (used by tests).
//! - Bounded default-off collection-service captures with exact trigger metadata.
//! - Request-owned full-sweep release of unused nursery payload and old holes.
//! - One-shot startup survival and post-major allocation budgets (see
//!   [`allocation_limits`]).
//! - [`GcHeap::embedder_root`] — strong handles the embedder keeps at fixed
//!   indices for code that holds only the heap.
//! - [`GcHeap::isolate_data`] — the embedder's per-isolate state, owned and
//!   rooted by the heap (see [`crate::isolate_data`]).
//! - [`MachineAllocationWindow`] — the audited nursery/accounting view exposed
//!   to generated-code allocators: the heap's linear allocation buffer, the
//!   old-space buffer of tenured allocation sites, and the per-type counters.
//! - Allocation-site placement ([`SitePlacement`]): tagged young cells and
//!   tenured old cells, with survival digested after every scavenge (see
//!   [`crate::pretenuring`]).
//! - Linear-allocation-buffer refill, retire and publication (see
//!   [`crate::lab`]).
//! - Ephemeron registry and split mark/sweep hooks used by weak
//!   collections in the VM.
//! - Weak-reference/finalization registry bookkeeping used by VM
//!   post-mark processing.
//! - Initialized trailing allocation
//!   ([`GcHeap::alloc_trailing_with_roots_initialized`]): a young body whose
//!   variable tail is filled before any collection can observe it.
//! - Stop-the-world mark drains and old-space sweeps of a large old
//!   generation run on the heap's helper threads (see `sweep`).
//!
//! # Invariants
//!
//! - Outside a marking cycle every cell is white: the sweep whitens each
//!   survivor, and only a cycle shades or allocates black. A cycle skips the
//!   whole-heap color reset unless an unswept cycle or a restored image left
//!   colors behind.
//! - The old generation is measured by the bytes its cells occupy, not the
//!   pages they span: the mark-sweep trigger and the limits it recomputes use
//!   the same measure, so swept holes count as free space.
//! - Every public `alloc<T>` records `T::TYPE_TAG` on the
//!   [`crate::header::GcHeader`]; full GC dispatches through the
//!   trace table by tag.
//! - Registered observation prologues run before nursery movement, each
//!   incremental marking step or sweep; observations cannot become strong roots.
//! - Owner-held weak references resolve after strong/ephemeron tracing and
//!   before nursery recycling or old-space sweep; resolution never marks.
//! - An initializer passed to an `_initialized` allocator runs at the body's
//!   final address with no intervening safepoint, before the old-placement
//!   barrier scan; the body is never traced with an unfilled tail.
//! - Black allocation: while a marking cycle is in progress, an old cell is
//!   born black and counted live (a buffer refilled then counts whole), and
//!   the edges it is initialized with pass the barrier; young cells are never
//!   marked. Old-generation marking runs concurrently on the helper threads
//!   from the cycle's start pause (a scavenge, the young survivors' old
//!   targets and the roots) to its finishing collection (another scavenge,
//!   the survivors, the roots again, then the rest of the drain and the
//!   sweep). A change [`GcHeap::with_payload`] makes to a visited cell while
//!   the mutator runs re-grays it for that final drain.
//! - Pages live forever inside the heap or are returned to the
//!   cage on full-GC sweep. Pages are never leaked across heap
//!   drops.
//! - Young allocation bumps the linear allocation buffer first. The buffer
//!   is retired before every collection, heap walk publication and direct
//!   nursery bump, and is refilled only when a plain young cell is legal, so
//!   the bump itself needs no policy test. The old-space buffer follows the
//!   same policy and is retired with it at every collection boundary.
//! - Successful collection refunds exactly reclaimed physical cells and
//!   released backing stores before publishing the surviving cap ledger. Minor
//!   collection uses its existing nursery counters; full collection sums bytes
//!   during its existing sweep, with no additional object census.
//! - Cap admission refunds unused charged LAB bytes only when that tail
//!   would otherwise reject allocation/reservation growth within the effective
//!   cap. Ordinary admitted growth retains the existing buffer.
//! - Weak collection tables are registered as type-erased raw
//!   handles. Every full collection runs the embedder's
//!   [`PostMarkProcessor`] exactly once between strong marking and sweep
//!   ([`GcHeap::collect_full`]), whatever triggered it: growth, heap cap or
//!   an explicit request. Split-phase callers run it through
//!   [`GcHeap::run_post_mark_processing`].
//! - After any collection that could have killed a weak target while
//!   finalization registries exist, [`GcHeap::take_weak_cleanup_due`]
//!   reports once that cleanup work may be pending.
//!
//! # See also
//!
//! - GC architecture plan §6.1 (unsafe boundary).

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::collection_goal::FullGcGoal;
use crate::compressed::{Cage, Gc, RawGc, cage_base};
use crate::ephemeron::EphemeronRegistry;
use crate::external::{ExternalMemory, SharedExternalMemory, SharedExternalState};
use crate::external_refs::ExternalRefTable;
use crate::extra_roots::{ExtraRoots, ExtraRootsGuard, RootSet};
use crate::finalize::WeakFinalizationRegistry;
use crate::frame_roots::{FrameRootProviders, FrameRoots, FrameRootsGuard};
use crate::handle::{GlobalHandleTable, HandleStack};
use crate::header::GcHeader;
use crate::lab::LinearAllocationArea;
use crate::marking::MarkingState;

use crate::observation::{
    GcPauseCapture, GcPauseCaptureError, GcPauseKind, GcPauseOutcome, GcPauseRecorder,
    GcPauseTrigger,
};
use crate::oom::OutOfMemory;
use crate::page::{CELL_SIZE, align_up};
use crate::page::{LARGE_OBJECT_THRESHOLD, PAGE_HEADER_SIZE, page_base_from_offset};
use crate::scavenger::ScavengeStats;
use crate::space::{LargeObjectSpace, NewSpace, OldSpace};
use crate::stats::GcStats;
use crate::store::GcStore;
use crate::trace::{TraceTable, Traceable};

#[path = "heap/allocation_limits.rs"]
mod allocation_limits;
use allocation_limits::{
    EXTERNAL_ALLOCATION_SOFT_LIMIT, INITIAL_OLD_GENERATION_SIZE, cage_hardcap_bytes,
    concurrent_marking_start,
};

/// Embedder weak-semantics pass run between strong marking and sweep.
///
/// It may mark additional objects (ephemeron values) and clear weak slots,
/// but must not allocate in this heap, reenter JavaScript or retain the heap.
pub type PostMarkProcessor = fn(&mut GcHeap);

/// Type alias for the higher-order visitor closure used by the
/// GC: it receives a `&mut dyn FnMut(*mut RawGc)` slot visitor
/// and emits one slot pointer per outgoing reference. Hoisted out
/// of the public method signatures to keep clippy's
/// `type_complexity` lint quiet.
pub type RootSlotVisitor<'a> = dyn FnMut(&mut dyn FnMut(*mut RawGc)) + 'a;

/// Weak-reference processing over one collection's forwarding: the callback
/// receives a resolver mapping a pre-collection cell to its survivor, or to
/// `None` when it died.
pub type WeakRefProcessor<'a> = dyn FnMut(&mut dyn FnMut(RawGc) -> Option<RawGc>) + 'a;

/// Empty root set — every reachable object must come from a
/// handle scope or a global handle.
pub fn empty() -> EmptyRoots {
    EmptyRoots
}

/// Number of [`GcHeap::embedder_root`] slots.
pub const EMBEDDER_ROOT_SLOTS: usize = 2;

/// Old-generation size from which a stop-the-world mark drain runs on the
/// helper threads too.
const PARALLEL_MARK_MIN_OLD_BYTES: u64 = 8 * 1024 * 1024;
/// Smallest overshoot of the old-generation limit that stops the world for
/// a mark-sweep rather than letting a concurrent cycle run on.
const LIMIT_OVERSHOOT_MARGIN_FOR_SMALL_HEAPS: u64 = 32 * 1024 * 1024;
/// How a fresh cell's trailing storage starts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TrailingInit {
    /// Zeroed before the initializer runs.
    Zeroed,
    /// Written whole by the initializer; only the padding after it is zeroed.
    Filled,
}

/// Bytes one group of cells was carved from the nursery buffer
/// ([`GcHeap::carve`]), placed front to back by [`GcHeap::place`].
#[must_use = "every carved byte must be placed"]
pub struct CellCarve {
    next: usize,
    end: usize,
}

impl CellCarve {
    /// Bytes not yet placed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.end - self.next
    }
}

impl Drop for CellCarve {
    /// Unplaced bytes become a free-space filler, so the page walk stays
    /// intact; a group places exactly what it carved.
    fn drop(&mut self) {
        if self.next < self.end {
            debug_assert!(false, "a group left carved bytes unplaced");
            // SAFETY: the bytes were handed out by the buffer and no cell
            // was written there.
            unsafe {
                (self.next as *mut GcHeader)
                    .write(GcHeader::new_free((self.end - self.next) as u32));
            }
        }
    }
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
    /// Bytes reclaimed across every full GC since the heap was created.
    /// Cumulative, so `allocated_bytes + total_full_reclaimed` is a lower
    /// bound on the bytes ever allocated even when repeated full GCs reclaim
    /// short-lived objects between two stats snapshots.
    pub total_full_reclaimed: usize,
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

/// Narrow nursery window published to audited generated-code allocators.
///
/// This is not a general heap-mutation API. A consumer may carve only
/// already-registered fixed-size bodies from [`Self::lab`] — bump `top` only
/// when the whole cell fits below `limit`. A carve does no accounting: the
/// heap derives the bytes generated code allocated from the buffer's
/// progress (see [`GcHeap::gc_stats`]). The buffer is empty whenever a
/// collector handshake is required, so a miss is the only policy signal.
#[doc(hidden)]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MachineAllocationWindow {
    /// The heap's linear allocation buffer, or the permanently empty one.
    pub lab: *mut LinearAllocationArea,
    /// The old-space buffer tenured allocation sites carve from, or the
    /// permanently empty one. Its cells are born old and initialized
    /// children are remembered by the carver.
    pub old_lab: *mut LinearAllocationArea,
}

/// Buffer a disabled window points at. Every bump misses, so generated code
/// never writes it.
static EMPTY_LAB: LinearAllocationArea = LinearAllocationArea::EMPTY;

impl MachineAllocationWindow {
    /// A window that forces the caller through its rooted cold path: its
    /// buffer never admits a cell.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            lab: std::ptr::addr_of!(EMPTY_LAB).cast_mut(),
            old_lab: std::ptr::addr_of!(EMPTY_LAB).cast_mut(),
        }
    }
}

/// Why a linear-allocation-buffer refill produced no window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LabRefill {
    /// The buffer now holds room for the requested cell.
    Ready,
    /// Plain young allocation is not legal now, or the cap refuses the tail.
    Declined,
    /// Every from-space page is full and the semispace cannot grow.
    NurseryFull,
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
    /// Mutator nursery bump window. Leads the struct: the allocation fast
    /// path reads nothing else from the heap.
    lab: LinearAllocationArea,
    /// Base address of the page [`Self::lab`] was carved from; `0` while
    /// the buffer is empty.
    lab_page: usize,
    /// Buffer start at refill; the page cursor and byte count lag `top`
    /// by exactly what the buffer handed out since then.
    lab_start: usize,
    /// Page `allocated_bytes` at refill.
    lab_page_allocated: usize,
    /// Bytes every retired buffer handed out, to the runtime allocator and
    /// to generated code alike.
    lab_consumed_bytes: u64,
    /// Bytes the runtime allocator itself bumped from buffers. The rest of
    /// [`Self::lab_consumed_bytes`] (plus the live buffer's progress) is what
    /// generated code allocated.
    runtime_lab_bytes: u64,
    /// Sums of the per-type rows' `alloc_count_total`, `alloc_bytes_total`
    /// and `live_bytes`, kept beside every row update so an accounting
    /// checkpoint reads them without walking the table.
    rows_alloc_count: u64,
    rows_alloc_bytes: u64,
    rows_live_bytes: usize,
    /// Cap bytes charged for the whole buffer at refill; the unused tail is
    /// refunded at retire. `0` without a cap.
    lab_charged: u64,
    /// Old-space buffer of tenured allocation sites; see [`tenured`].
    old_lab: LinearAllocationArea,
    /// Cap bytes charged for the whole old buffer at refill. `0` without a cap.
    old_lab_charged: u64,
    /// Survival feedback and decisions of the isolate's allocation sites.
    allocation_sites: crate::pretenuring::AllocationSites,
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
    /// Young large cells: counted against the nursery's capacity, kept in
    /// place by a scavenge, which moves a survivor's page to
    /// [`Self::large_space`] and releases the rest.
    young_large_space: LargeObjectSpace,
    /// Bytes the scavenges since the nursery's last capacity growth kept
    /// alive; past one capacity's worth the capacity doubles.
    survived_since_last_expansion: usize,
    trace_table: TraceTable,
    /// Isolate-local interner for the non-GC addresses bodies point at.
    /// Per-heap, not process-global, so index order is a property of
    /// this isolate's install sequence alone.
    external_refs: std::sync::Arc<crate::external_refs::SharedExternalRefs>,
    /// Compiler threads' right to read this heap; every pause takes it.
    read_right: std::sync::Arc<crate::read_right::HeapReadRight>,
    /// Owned host payloads (dynamic-native closures) bodies name by
    /// index. See [`crate::host_refs`]; slots are recycled from the
    /// sweep through [`crate::trace::ReleaseHostRefs`].
    host_refs: crate::host_refs::HostRefTable,
    marking: MarkingState,
    /// Object-granular remembered set: the per-isolate store buffer of
    /// mutated **old/large parents** that hold an old→young edge. Fed by
    /// the write barrier (deduped by `GcHeader` `FLAG_REMEMBERED`) and
    /// consumed as additional roots by every minor scavenge, replacing the
    /// card-table dirty-page header walk. Holds in-cage `RawGc` parent
    /// offsets only; old/large objects do not move on a minor GC, so the
    /// offsets stay valid across the scavenge that drains them. A full GC
    /// prunes dead parents from it after sweep.
    remembered_parents: Vec<RawGc>,
    handle_stack: Box<HandleStack>,
    global_handles: Box<GlobalHandleTable>,
    /// Strong handles the embedder keeps at fixed indices for code that holds
    /// only the heap. Traced as roots and rewritten in place when their
    /// targets move.
    embedder_roots: [Cell<RawGc>; EMBEDDER_ROOT_SLOTS],
    /// The embedder's per-isolate state; its old-space handles are marking
    /// roots.
    isolate_data: Option<crate::isolate_data::IsolateDataSlot>,
    /// LIFO stack of runtime-owned root sources. A stack — not a
    /// single slot — because VM scopes nest (native call → re-entrant
    /// dispatch → nested native): replacing a single registration
    /// would hide the outer scope's roots from any collection
    /// triggered inside the inner scope, leaving moved objects
    /// unreachable through the outer scope's Rust-local handles.
    /// Every live entry is traced; duplicates (the same interpreter
    /// re-registered by a nested scope) are skipped via
    /// [`ExtraRoots::same_source`].
    extra_roots: Vec<ExtraRoots>,
    frame_root_providers: FrameRootProviders,
    ephemerons: EphemeronRegistry,
    weak_finalization: WeakFinalizationRegistry,
    /// Weak-semantics pass installed by the owning VM; see [`PostMarkProcessor`].
    post_mark: Option<PostMarkProcessor>,
    /// Set after a collection while finalization registries exist; the VM
    /// consumes it at its next job checkpoint to look for emptied cells.
    weak_cleanup_due: bool,
    /// Default-off bounded collection-service observation; no heap values.
    pause_recorder: GcPauseRecorder,
    shared_external: Arc<SharedExternalState>,
    /// Optional shared-ledger mirror of [`Self::reserved_bytes`] as
    /// `ExternalBytes`. Growth is admitted on the ledger before any heap
    /// state changes; shrinks follow every release path, including drained
    /// shared-token releases. `None` until the runtime installs its account.
    external_ledger: Option<otter_resource::ResourceLease>,
    /// Old-generation allocation limit over occupied old/large bytes. The
    /// default starts at [`INITIAL_OLD_GENERATION_SIZE`], first successful
    /// nursery survival seeds it once, and every mark-compact recomputes it
    /// from the surviving cells and measured collection/allocation speeds
    /// (see [`allocation_limits`]). Without it a cap-less heap never
    /// runs a full GC, so old space — fed by scavenge promotion and the
    /// nursery-overflow path — grows unbounded until the cage or the cap is
    /// exhausted even when most of it is garbage.
    old_generation_limit: u64,
    /// The default startup budget has not yet consumed nursery survival,
    /// an explicit override or a completed major controller update.
    initial_old_limit_pending: bool,
    /// Old-generation occupancy at which a concurrent marking cycle starts,
    /// so that it can finish before [`Self::old_generation_limit`].
    concurrent_marking_start: u64,
    /// Helper threads that trace and copy alongside the collecting thread,
    /// started at the first scavenge large enough to share.
    gc_threads: crate::gc_threads::GcThreads,
    /// Every cell is white: no marking cycle has run since the last sweep
    /// whitened its survivors, and no restored page brought colors in. A
    /// cycle starting while this holds skips the whole-heap color reset.
    marks_white: bool,
    /// The old-space sweep of the last full collection while its pages are
    /// still being walked behind the pause; see [`sweeping`].
    sweeping: Option<sweeping::SweepCycle>,
    /// External reservation bytes that force a mark-compact: the surviving
    /// reservations plus [`EXTERNAL_ALLOCATION_SOFT_LIMIT`].
    external_limit: u64,
    /// Old/large page bytes the last mark-compact left, the base of the
    /// allocation throughput the next one measures.
    old_generation_bytes_at_last_gc: u64,
    /// When the last mark-compact ended (or the heap was created).
    last_major_gc_end: Instant,
    /// Total scavenge pause time when the last mark-compact ended; pauses
    /// since then are not mutator time.
    minor_pause_ns_at_last_major_gc: u64,
    /// Re-entrancy guard: `true` while a growth-triggered major GC
    /// is running, so the minor scavenge it performs internally
    /// cannot recursively trigger another one.
    in_major_gc: bool,
    /// GC-stress ("gc-zeal") stride: when non-zero, force a scavenge
    /// every `gc_stress_stride` allocations. Moving every young object
    /// that often makes any use-after-move bug — a `Value`/`Gc<T>`
    /// held in a Rust local across an allocation without being rooted
    /// — fail deterministically in tests instead of intermittently in
    /// production. Off by default; enabled via `OTTER_GC_STRESS=<n>`
    /// (and `=full` to also force a major GC each time). The runtime
    /// counterpart to a static rooting-hazard lint.
    /// Route ordinary allocations straight to old-space.
    ///
    /// Set while the runtime builds its globals: everything allocated then is
    /// permanent, so copying it through the nursery buys nothing and costs a
    /// scavenge — plus every later scavenge re-traces it until it is promoted.
    tenure_all: bool,
    gc_stress_stride: u32,
    /// Also force a full GC (not just a scavenge) on each stress tick.
    gc_stress_full: bool,
    /// Allocations since the last stress-triggered collection.
    gc_stress_counter: u32,
    /// Stress is active from heap construction. Bootstrap and user code obey
    /// the same rooting contract; delaying stress would hide initialization
    /// roots that are just as load-bearing as runtime roots.
    gc_stress_armed: bool,
    /// Open [`AlwaysAllocateScope`]s. While non-zero, allocation never starts
    /// a collection: a full nursery overflows into old space, due major and
    /// stress collections wait, and the byte cap is checked without an
    /// emergency collection.
    always_allocate_depth: u32,
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

/// RAII guard returned by [`GcHeap::always_allocate_scope`].
#[must_use = "dropping the scope immediately re-enables collection"]
pub struct AlwaysAllocateScope {
    heap: *mut GcHeap,
}

impl Drop for AlwaysAllocateScope {
    fn drop(&mut self) {
        // SAFETY: the heap outlives every scope opened on it; scopes nest.
        unsafe {
            (*self.heap).always_allocate_depth =
                (*self.heap).always_allocate_depth.saturating_sub(1);
        }
    }
}

/// Debug-only stale-handle trap. A live handle is always updated to its
/// to-space copy by the collector, so a mutator payload access that lands on
/// a *forwarded* cell means a raw `Gc<T>` was held across an allocation that
/// scavenged — a use-after-move rooting bug that is *never* legitimate.
/// Trapping here names the type and condition at the offending read instead
/// of leaving a cryptic downstream shape/tag corruption.
///
/// The stricter *swept* (use-after-free) check reads dropped old-space memory;
/// a handful of low-level collector tests deliberately probe non-moving
/// old-space cells after a full GC, so it is gated behind `OTTER_GC_VERIFY=1`
/// alongside the other opt-in scavenge diagnostics.
///
/// Compiled out entirely in release builds. Type-tag mismatch is left to the
/// existing per-payload asserts; this only checks liveness.
#[inline]
fn debug_assert_handle_live<T: Traceable>(handle: Gc<T>, context: &str) {
    if cfg!(debug_assertions) && !handle.is_null() {
        // SAFETY: header read only (no payload touch); single mutator, STW GC.
        unsafe {
            let header = &*handle.as_header_ptr();
            debug_assert!(
                !header.is_forwarded(),
                "use-after-move: {context} on a forwarded {} cell (tag={:#x}) — a raw Gc handle \
                 was held across a scavenging allocation and never re-read from its rooted slot",
                std::any::type_name::<T>(),
                header.type_tag(),
            );
            if gc_verify_use_after_free_enabled() {
                debug_assert!(
                    !header.is_swept(),
                    "use-after-free: {context} on a swept {} cell (tag={:#x}) — the handle \
                     outlived a full collection that finalized and dropped the payload",
                    std::any::type_name::<T>(),
                    header.type_tag(),
                );
            }
        }
    }
}

/// Cached `OTTER_GC_VERIFY` gate for the opt-in use-after-free trap. Read once;
/// a per-read env lookup would dominate the debug-build hot path.
fn gc_verify_use_after_free_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0);
    match STATE.load(Ordering::Relaxed) {
        0 => {
            let on = std::env::var_os("OTTER_GC_VERIFY").is_some_and(|v| v != "0");
            STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
        2 => true,
        _ => false,
    }
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
        // GC-stress ("gc-zeal") config from the environment. `OTTER_GC_STRESS=1`
        // scavenges every allocation (maximum use-after-move detection, very
        // slow); `=<n>` every n allocations; append/set `=full` to make the
        // ticks alternate between starting a concurrent marking cycle and the
        // major GC that finishes it. Unset/`0` → disabled (the production default).
        let (gc_stress_stride, gc_stress_full) = match std::env::var("OTTER_GC_STRESS") {
            Ok(v) => {
                let v = v.trim().to_ascii_lowercase();
                let full = v.contains("full");
                let stride = if v == "full" || v.is_empty() {
                    1
                } else {
                    v.trim_end_matches("full")
                        .trim_end_matches(['=', ',', ':'])
                        .trim()
                        .parse::<u32>()
                        .unwrap_or(1)
                };
                (stride, full)
            }
            Err(_) => (0, false),
        };
        Ok(Self {
            lab: LinearAllocationArea::EMPTY,
            lab_page: 0,
            lab_start: 0,
            lab_page_allocated: 0,
            lab_consumed_bytes: 0,
            runtime_lab_bytes: 0,
            rows_alloc_count: 0,
            rows_alloc_bytes: 0,
            rows_live_bytes: 0,
            lab_charged: 0,
            old_lab: LinearAllocationArea::EMPTY,
            old_lab_charged: 0,
            allocation_sites: crate::pretenuring::AllocationSites::default(),
            new_space,
            tenure_all: false,
            old_space: OldSpace::new(),
            large_space: LargeObjectSpace::new(),
            young_large_space: LargeObjectSpace::new_young(),
            survived_since_last_expansion: 0,
            trace_table: TraceTable::new(),
            external_refs: std::sync::Arc::default(),
            read_right: std::sync::Arc::default(),
            host_refs: crate::host_refs::HostRefTable::default(),
            marking: MarkingState::new(),
            remembered_parents: Vec::new(),
            handle_stack: Box::new(HandleStack::new()),
            global_handles: Box::new(GlobalHandleTable::new()),
            embedder_roots: Default::default(),
            isolate_data: None,
            extra_roots: Vec::new(),
            frame_root_providers: FrameRootProviders::new(),
            ephemerons: EphemeronRegistry::default(),
            weak_finalization: WeakFinalizationRegistry::default(),
            post_mark: None,
            weak_cleanup_due: false,
            pause_recorder: GcPauseRecorder::default(),
            shared_external: Arc::new(SharedExternalState::default()),
            external_ledger: None,
            stats: HeapStats::default(),
            gc_stats: GcStats::default(),
            max_heap_bytes: cap,
            tracked_bytes: 0,
            reserved_bytes: 0,
            old_generation_limit: INITIAL_OLD_GENERATION_SIZE.min(cage_hardcap_bytes()),
            initial_old_limit_pending: true,
            concurrent_marking_start: concurrent_marking_start(
                0,
                INITIAL_OLD_GENERATION_SIZE.min(cage_hardcap_bytes()),
            ),
            gc_threads: crate::gc_threads::GcThreads::default(),
            marks_white: true,
            sweeping: None,
            external_limit: EXTERNAL_ALLOCATION_SOFT_LIMIT,
            old_generation_bytes_at_last_gc: 0,
            last_major_gc_end: Instant::now(),
            minor_pause_ns_at_last_major_gc: 0,
            in_major_gc: false,
            gc_stress_stride,
            gc_stress_full,
            gc_stress_counter: 0,
            gc_stress_armed: gc_stress_stride != 0,
            always_allocate_depth: 0,
            oom_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Register an active interpreter frame-stack root provider.
    ///
    /// Returns the new provider depth; callers pass `depth - 1` to
    /// [`Self::pop_frame_roots_to`] when leaving the matching dispatch scope.
    #[doc(hidden)]
    pub fn push_frame_roots(&mut self, provider: *const dyn FrameRoots) -> usize {
        self.frame_root_providers.push(provider)
    }

    /// Register active interpreter/JIT frame roots until the guard drops.
    pub fn register_frame_roots(&mut self, provider: *const dyn FrameRoots) -> FrameRootsGuard {
        let depth = self.push_frame_roots(provider) - 1;
        FrameRootsGuard::new(self, depth)
    }

    /// Open one root-scope marker and return its provider index.
    pub(crate) fn push_root_scope(&mut self) -> usize {
        self.frame_root_providers.push_scope() - 1
    }

    /// Root `slot` for the open root scope at provider index `owner`.
    pub(crate) fn push_root_scope_slot(
        &mut self,
        owner: usize,
        slot: *mut (),
        tracer: crate::root_scope::ErasedSlotTracer,
    ) {
        self.frame_root_providers
            .push_scope_slot(owner, slot, tracer);
    }

    /// Whether an allocation-driven stress collection may run now.
    fn stress_collection_armed(&self) -> bool {
        self.gc_stress_stride != 0
            && self.gc_stress_armed
            && !self.in_major_gc
            && self.always_allocate_depth == 0
    }

    /// Open a scope in which allocation never starts a collection.
    ///
    /// For VM work that must allocate while some mutator values are held
    /// where no collector can see them, such as a compile requested from a
    /// generated loop's back-edge poll whose live registers are not rooted.
    /// Allocations still fail with an ordinary [`OutOfMemory`] when the cage
    /// or the byte cap cannot satisfy them.
    pub fn always_allocate_scope(&mut self) -> AlwaysAllocateScope {
        self.always_allocate_depth = self.always_allocate_depth.saturating_add(1);
        AlwaysAllocateScope { heap: self }
    }

    /// Truncate active frame-root providers back to `depth`.
    #[doc(hidden)]
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

    /// Shared release channel handed to external reservation tokens.
    pub(crate) fn shared_external_state(&self) -> Arc<SharedExternalState> {
        Arc::clone(&self.shared_external)
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
    fn admit_external_growth(&mut self, bytes: u64) -> Result<(), OutOfMemory> {
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
    fn shrink_external_ledger(&mut self, bytes: u64) {
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
    fn can_admit_with_roots(
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
    fn account_or_collect_with_roots(
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
    fn account_or_collect(&mut self, bytes: u64) -> Result<(), OutOfMemory> {
        // External root visitor is empty — the handle stack and
        // global handle table are walked from inside
        // `collect_full`, and the embedder has no opportunity to
        // re-enter the heap during a refused allocation.
        let mut noop = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.account_or_collect_with_roots(bytes, &mut noop)
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

    /// Whether a header read at `offset` could belong to a live body.
    /// See [`TraceTable::header_could_be_live`].
    #[must_use]
    pub fn header_could_be_live(&self, offset: u32, size_bytes: u32, type_tag: u8) -> bool {
        self.trace_table
            .header_could_be_live(offset, size_bytes, type_tag)
    }

    /// Reference to the trace table (used by tests).
    #[doc(hidden)]
    pub fn trace_table(&self) -> &TraceTable {
        &self.trace_table
    }

    /// Intern a non-GC address so a body can reference it by a small
    /// index instead of storing the raw pointer. See
    /// [`crate::external_refs`].
    pub fn intern_external_ref(&mut self, addr: usize) -> u32 {
        self.external_refs.with_mut(|table| table.intern(addr))
    }

    /// The heap's external-reference table.
    #[must_use]
    pub fn external_refs(&self) -> &ExternalRefTable {
        self.external_refs.get()
    }

    /// Compiler threads' right to read this heap.
    #[must_use]
    pub fn read_right(&self) -> std::sync::Arc<crate::read_right::HeapReadRight> {
        std::sync::Arc::clone(&self.read_right)
    }

    /// The external-reference table, for compiler threads.
    #[must_use]
    pub fn shared_external_refs(&self) -> std::sync::Arc<crate::external_refs::SharedExternalRefs> {
        std::sync::Arc::clone(&self.external_refs)
    }

    /// Rebuild the external-reference table from a same-process captured
    /// address list, preserving indices. See
    /// [`ExternalRefTable::restore_from_addrs`].
    pub fn restore_external_refs(&mut self, addrs: impl IntoIterator<Item = usize>) {
        self.external_refs
            .with_mut(|table| table.restore_from_addrs(addrs));
    }

    /// Store an owned host payload and return its index. See
    /// [`crate::host_refs`].
    pub fn intern_host_ref(&mut self, payload: Box<dyn std::any::Any>) -> u32 {
        self.host_refs.insert(payload)
    }

    /// The heap's host-reference table.
    #[must_use]
    pub fn host_refs(&self) -> &crate::host_refs::HostRefTable {
        &self.host_refs
    }

    /// The heap's host-reference table, mutably. Restore-path use:
    /// payloads must land at the indices the captured bodies carry.
    pub fn host_refs_mut(&mut self) -> &mut crate::host_refs::HostRefTable {
        &mut self.host_refs
    }

    /// Register a [`crate::trace::ReleaseHostRefs`] body so the sweep
    /// releases its host-ref slots when it dies. Must be paired with an
    /// earlier `register_traceable::<T>()`.
    pub fn register_host_release<T: Traceable + crate::trace::ReleaseHostRefs>(&mut self) {
        self.trace_table.register_host_release::<T>();
    }

    /// Register the restore-time foreign-ownership sever hook. See
    /// [`crate::trace::SeverRestoredPayload`].
    pub fn register_sever_restored<T: Traceable + crate::trace::SeverRestoredPayload>(&mut self) {
        self.trace_table.register_sever_restored::<T>();
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

    /// The handle in embedder root slot `index`; null until set.
    #[must_use]
    pub fn embedder_root(&self, index: usize) -> RawGc {
        self.embedder_roots[index].get()
    }

    /// Keep `raw` alive in embedder root slot `index`.
    pub fn set_embedder_root(&self, index: usize, raw: RawGc) {
        self.embedder_roots[index].set(raw);
    }

    /// Visit every embedder root slot, filled or not, in index order: the
    /// fixed-shape walk a heap snapshot captures and restores.
    pub fn visit_embedder_root_slots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for slot in &self.embedder_roots {
            visitor(slot.as_ptr());
        }
    }

    /// Take ownership of the embedder's per-isolate state for the heap's
    /// life. Its old-space handles are roots of every marking from here on.
    ///
    /// # Panics
    /// When the heap already holds a value.
    pub fn install_isolate_data<T: crate::isolate_data::IsolateData>(&mut self, data: T) {
        assert!(
            self.isolate_data.is_none(),
            "a heap owns one isolate data value"
        );
        self.isolate_data = Some(crate::isolate_data::IsolateDataSlot::new(data));
    }

    /// The per-isolate state, when it was installed as `T`.
    #[inline]
    #[must_use]
    pub fn isolate_data<T: crate::isolate_data::IsolateData>(&self) -> Option<&T> {
        self.isolate_data.as_ref()?.get()
    }

    /// [`Self::isolate_data`] for mutation.
    #[inline]
    pub fn isolate_data_mut<T: crate::isolate_data::IsolateData>(&mut self) -> Option<&mut T> {
        self.isolate_data.as_mut()?.get_mut()
    }

    /// Visit the filled embedder root slots.
    fn trace_embedder_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for slot in &self.embedder_roots {
            if !slot.get().is_null() {
                visitor(slot.as_ptr());
            }
        }
    }

    /// Push an extra runtime root source onto the heap's LIFO
    /// registration stack.
    ///
    /// Returns the new stack depth; callers pass `depth - 1` to
    /// [`Self::pop_extra_roots_to`] when the source goes out of scope.
    /// Every live entry is traced during collection, so a nested
    /// registration never hides an outer scope's roots.
    #[doc(hidden)]
    #[must_use]
    #[inline]
    pub fn push_extra_roots(&mut self, roots: ExtraRoots) -> usize {
        self.extra_roots.push(roots);
        self.extra_roots.len()
    }

    /// Register an owner-managed root source until the returned guard drops.
    ///
    /// This is the standard API for VM/runtime root owners. It keeps cleanup
    /// panic-safe while allowing normal mutable heap access inside the scope.
    #[inline]
    pub fn register_extra_roots(&mut self, roots: ExtraRoots) -> ExtraRootsGuard {
        let depth = self.push_extra_roots(roots) - 1;
        ExtraRootsGuard::new(self, depth)
    }

    /// Whether any extra-roots provider is registered. The interpreter installs
    /// one (over its full runtime root set) for the duration of the dispatch
    /// loop, so a collection pulls those roots itself — callers that would
    /// otherwise snapshot the runtime root set on every allocation can skip it.
    #[must_use]
    pub fn has_extra_roots(&self) -> bool {
        !self.extra_roots.is_empty()
    }

    /// Whether `roots` is already registered as this exact source.
    ///
    /// A caller that needs one particular owner (rather than merely any
    /// transient root provider) uses this to avoid both an unsound false
    /// positive and a duplicate registration. Source identity includes the
    /// concrete dispatch thunk, so equal data addresses of different source
    /// types do not alias.
    #[must_use]
    pub fn has_extra_root_source(&self, roots: ExtraRoots) -> bool {
        self.extra_roots
            .iter()
            .any(|registered| registered.same_source(&roots))
    }

    /// Truncate the extra-roots stack back down to `depth`.
    #[doc(hidden)]
    #[inline]
    pub fn pop_extra_roots_to(&mut self, depth: usize) {
        debug_assert!(depth <= self.extra_roots.len());
        self.extra_roots.truncate(depth);
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
    fn sync_lab(&self) {
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
    fn retire_machine_labs(&mut self) {
        self.retire_lab();
        self.retire_old_lab();
    }

    /// Publish the buffer into its page and drop it, refunding the unused
    /// tail to the heap cap.
    fn retire_lab(&mut self) {
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
    fn refill_lab(&mut self, aligned: usize) -> LabRefill {
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

    /// Write a young cell carved from the linear allocation buffer at
    /// `address` and return its handle.
    #[inline(always)]
    fn publish_lab_cell<T: Traceable>(
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

    fn try_alloc_trailing_no_collect_as<T: Traceable>(
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
    fn alloc_trailing_with_roots_inner<T: Traceable>(
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
    fn alloc_trailing_slow<T: Traceable>(
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
    fn alloc_old_with_roots_inner<T: Traceable>(
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

    /// Begin a bounded collection-service capture at a quiescent heap boundary.
    /// Storage is reserved now; collection never allocates observation records.
    pub fn start_gc_pause_capture(&mut self, capacity: usize) -> Result<(), GcPauseCaptureError> {
        if self.marking.is_marking() && !self.pause_recorder.is_capturing() {
            return Err(GcPauseCaptureError::CollectionInProgress);
        }
        self.pause_recorder.start(capacity)
    }

    /// Stop capture and move out owned scalar records, including overflow and
    /// incomplete/unsupported split-phase metadata. No heap walk is performed.
    pub fn take_gc_pause_capture(&mut self) -> Result<GcPauseCapture, GcPauseCaptureError> {
        self.pause_recorder.take()
    }

    fn with_gc_pause<R>(
        &mut self,
        kind: GcPauseKind,
        trigger: GcPauseTrigger,
        work: impl FnOnce(&mut Self) -> (R, GcPauseOutcome),
    ) -> R {
        // No compiler thread reads the heap while it changes under a pause.
        let right = std::sync::Arc::clone(&self.read_right);
        let _exclusive = right.exclude();
        crate::compressed::age_retained_pages(Instant::now());
        let token = self.pause_recorder.begin(
            kind,
            trigger,
            self.gc_stats.gc_cycles,
            self.gc_stats.minor_gc_cycles,
            self.observed_occupancy(),
        );
        let (result, outcome) = work(self);
        let occupancy = self.observed_occupancy();
        self.pause_recorder.end(
            token,
            outcome,
            self.gc_stats.gc_cycles,
            self.gc_stats.minor_gc_cycles,
            occupancy,
        );
        result
    }

    /// Occupancy for a captured pause record; nothing is read while no
    /// capture runs.
    fn observed_occupancy(&self) -> crate::observation::HeapOccupancy {
        if !self.pause_recorder.is_capturing() {
            return crate::observation::HeapOccupancy::default();
        }
        crate::observation::HeapOccupancy {
            old_bytes: self.old_generation_page_bytes(),
            old_allocated_bytes: (self.old_space.allocated_bytes()
                + self.large_space.allocated_bytes()) as u64,
            young_bytes: (self.new_space.allocated_bytes()
                + self.young_large_space.allocated_bytes()) as u64,
        }
    }

    /// Hand over the pause records captured so far and keep capturing.
    /// `None` when no capture runs or a collection is in progress.
    pub fn drain_gc_pause_capture(&mut self) -> Option<GcPauseCapture> {
        self.pause_recorder.drain()
    }

    fn with_gc_pause_result<R>(
        &mut self,
        kind: GcPauseKind,
        trigger: GcPauseTrigger,
        work: impl FnOnce(&mut Self) -> Result<R, OutOfMemory>,
    ) -> Result<R, OutOfMemory> {
        self.with_gc_pause(kind, trigger, |heap| {
            let full_before = heap.gc_stats.gc_cycles;
            let result = work(heap);
            let outcome = if result.is_ok() {
                GcPauseOutcome::Completed
            } else if trigger == GcPauseTrigger::HeapCap
                && heap.gc_stats.gc_cycles > full_before
                && matches!(result, Err(OutOfMemory::HeapCapExceeded { .. }))
            {
                GcPauseOutcome::CompletedAllocationRefused
            } else {
                GcPauseOutcome::CollectionFailed
            };
            (result, outcome)
        })
    }

    /// Run a minor GC (Cheney scavenge).
    pub fn collect_minor(&mut self, _roots: EmptyRoots) -> Result<(), OutOfMemory> {
        let empty: fn(&mut dyn FnMut(*mut RawGc)) = |_| {};
        self.collect_minor_internal(&mut |v| empty(v))
    }

    /// A young large-object page for `aligned` bytes, when the nursery
    /// admits it: the young large cells already allocated plus this one stay
    /// within the nursery's capacity, a scavenge first making room when they
    /// would not. `None` places the body in the old large object space: a
    /// body larger than the nursery itself, or an allocation inside a
    /// collection.
    fn admit_young_large(
        &mut self,
        aligned: usize,
        allocation_roots: &mut RootSlotVisitor<'_>,
    ) -> Result<Option<u32>, OutOfMemory> {
        let capacity = self.new_space.capacity_bytes();
        if self.always_allocate_depth != 0 || aligned > capacity {
            return Ok(None);
        }
        if self.young_large_space.allocated_bytes() + aligned > capacity {
            self.collect_minor_observed(allocation_roots, GcPauseTrigger::NurseryCapacity)?;
        }
        Ok(Some(self.young_large_space.alloc(aligned)?))
    }

    /// After a scavenge: the young large cells it promoted move with their
    /// pages to the old large object space, and every other young large
    /// page is released after its body's reclamation hooks run. Returns the
    /// released bytes.
    fn finish_young_large(&mut self) -> usize {
        let trace_table = &self.trace_table;
        let host_refs = &mut self.host_refs;
        self.young_large_space
            .promote_or_release(&mut self.large_space, |page| {
                let mut promoted = false;
                // SAFETY: STW after the scavenge; the page holds one
                // initialized cell of a registered type.
                unsafe {
                    page.for_each_object(|header, _| {
                        if !(*header).is_young() {
                            promoted = true;
                            return;
                        }
                        let tag = (*header).type_tag();
                        if let Some(release) = trace_table.get_host_release(tag) {
                            release(header, host_refs);
                        }
                        if let Some(finalize) = trace_table.get_finalize(tag) {
                            finalize(header);
                        }
                        if let Some(drop_fn) = trace_table.get_drop(tag) {
                            drop_fn(header);
                        }
                    });
                }
                promoted
            })
    }

    /// Run a minor GC with caller-supplied root visitor.
    pub fn collect_minor_with_roots(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.collect_minor_internal(external_visit)
    }

    fn prepare_collection_observations(&self) {
        for (index, source) in self.extra_roots.iter().enumerate() {
            if !self.extra_roots[..index]
                .iter()
                .any(|prior| prior.same_source(source))
            {
                source.prepare_collection(self);
            }
        }
    }

    fn collect_minor_internal(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.collect_minor_observed(external_visit, GcPauseTrigger::Explicit)
    }

    fn collect_minor_observed(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
        trigger: GcPauseTrigger,
    ) -> Result<(), OutOfMemory> {
        self.with_gc_pause_result(GcPauseKind::Minor, trigger, |heap| {
            heap.collect_minor_work(external_visit)
        })
    }

    fn collect_minor_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        // The scavenge needs the helpers and promotes into old space.
        self.complete_old_sweep();
        // A concurrent marker never reads young cells, but the scavenge needs
        // the helpers; it resumes once the nursery is evacuated.
        let resume_marking = self.marking.is_concurrent();
        self.marking.stop_concurrent(&mut self.gc_threads);
        let marking = self.marking.is_marking();
        let mut promoted_gray = Vec::new();
        self.retire_machine_labs();
        self.prepare_collection_observations();
        // Combine the caller's external_visit with the heap's
        // own handle-stack and global-handles walk.
        let handle_stack: *const HandleStack = &*self.handle_stack;
        let global_handles: *const GlobalHandleTable = &*self.global_handles;
        let extra_roots: *const Vec<ExtraRoots> = &self.extra_roots;
        let frame_root_providers: *const FrameRootProviders = &self.frame_root_providers;
        let embedder_roots: *const Self = self;
        let mut combined = move |visitor: &mut dyn FnMut(*mut RawGc)| {
            // SAFETY: STW pause; raw pointers reconstituted to
            // shared references.
            unsafe {
                (*handle_stack).visit_slots(visitor);
                (*global_handles).visit_slots(visitor);
                (*embedder_roots).trace_embedder_roots(visitor);
            }
            external_visit(visitor);
            // SAFETY: STW pause; the heap owns the registration stack for the
            // duration of this root walk.
            visit_extra_roots_deduped(unsafe { &*extra_roots }, RootSet::Young, visitor);
            // SAFETY: STW pause; the heap owns the registry for the duration of
            // this root walk.
            unsafe { (*frame_root_providers).trace(visitor) };
        };
        let ephemeron_registry_slots = self.ephemerons.handle_slots();
        let weak_registry_slots = self.weak_finalization.handle_slots();
        let mut process_weak = |resolve: &mut dyn FnMut(RawGc) -> Option<RawGc>| {
            // SAFETY: the registration stack is unchanged during this pause;
            // no weak owner may allocate or change registrations.
            let sources = unsafe { &*extra_roots };
            for (index, source) in sources.iter().enumerate() {
                if !sources[..index]
                    .iter()
                    .any(|prior| prior.same_source(source))
                {
                    source.process_weak(resolve);
                }
            }
        };
        // SAFETY: STW pause for the duration of the call;
        // every type tag in from-space is registered.
        let scavenge_start = Instant::now();
        let nursery_at_maximum = self.new_space.at_maximum();
        let mut stats = unsafe {
            crate::scavenger::scavenge(
                &mut self.new_space,
                &mut self.old_space,
                &self.trace_table,
                &[],
                &mut combined,
                &ephemeron_registry_slots,
                &weak_registry_slots,
                &mut process_weak,
                &mut self.remembered_parents,
                &mut self.host_refs,
                &mut self.allocation_sites,
                &mut self.gc_threads,
                marking.then_some(&mut promoted_gray),
            )
        }?;
        for raw in promoted_gray {
            // SAFETY: the scavenge promoted this cell gray just now.
            unsafe { self.marking.push_gray(raw.as_header_ptr()) };
        }
        let released_large = self.finish_young_large();
        stats.reclaimed_bytes += released_large;
        // The nursery grows once the scavenges since its last growth kept
        // a whole capacity alive: its objects outlive the space they get.
        self.survived_since_last_expansion +=
            stats.copied_bytes + stats.promoted_bytes + stats.promoted_large_bytes;
        if !self.new_space.at_maximum()
            && self.survived_since_last_expansion > self.new_space.capacity_bytes()
        {
            self.new_space.grow_capacity();
            self.survived_since_last_expansion = 0;
        }
        self.allocation_sites.digest(nursery_at_maximum);
        stats.minor_pause_ns = scavenge_start.elapsed().as_nanos() as u64;
        self.ephemerons.retain_non_null();
        self.weak_finalization.retain_non_null();
        self.weak_cleanup_due |= self.weak_finalization.has_finalization_registries();
        if self.max_heap_bytes != 0 {
            // Successful evacuation preserves copied/promoted bytes; only
            // doomed physical nursery cells disappear. Failed preflight never
            // reaches this commit. Payload token drops are drained separately.
            self.tracked_bytes = self
                .tracked_bytes
                .saturating_sub(stats.reclaimed_bytes as u64);
            self.drain_shared_external_releases();
        }
        self.stats.last_scavenge = stats;
        self.gc_stats.record_minor(&stats);
        self.seed_initial_old_generation_limit(&stats);
        // Per-tag counters drift between scavenges (young
        // allocations counted at alloc-time but never
        // decremented when the scavenger reclaims them); the
        // drift is corrected on the next full GC via
        // [`Self::reconcile_live_counts`]. Scavenge keeps the
        // hot path tight by not walking the live set just for
        // stats — see `bench_scavenge_4mb` for the cost of
        // adding a walk here.
        if resume_marking {
            // SAFETY: the cycle is still active; the scavenge is complete.
            unsafe {
                self.marking
                    .start_concurrent(&self.trace_table, &mut self.gc_threads)
            };
        }
        Ok(())
    }

    /// Run an explicit full GC and release unused physical payload.
    ///
    /// `external_visit` yields every external root slot.
    pub fn collect_full(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.collect_full_observed(
            external_visit,
            GcPauseTrigger::Explicit,
            FullGcGoal::ReleaseMemory,
        )
    }

    /// Collect with the owning request's physical-memory goal and exact trigger
    /// provenance. Both goals use the same collector and canonical root visitors;
    /// capture state and trigger labels never choose the goal.
    pub fn collect_full_observed(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
        trigger: GcPauseTrigger,
        goal: FullGcGoal,
    ) -> Result<(), OutOfMemory> {
        self.with_gc_pause_result(GcPauseKind::Full, trigger, |heap| {
            heap.collect_full_work(external_visit, goal)
        })
    }

    fn collect_full_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
        goal: FullGcGoal,
    ) -> Result<(), OutOfMemory> {
        self.complete_old_sweep();
        let pause_start = Instant::now();
        let old_generation_before = self.old_generation_consumed_bytes();
        // A cycle already marking, concurrently or between incremental
        // steps, finishes; otherwise the whole mark runs in this pause.
        if self.marking.is_marking() {
            self.finish_incremental_mark_phase(external_visit)?;
        } else {
            self.mark_phase(external_visit)?;
        }
        self.run_post_mark_processing();
        self.sweep_phase_with_pause_start(pause_start, goal, Some(old_generation_before));
        Ok(())
    }

    /// Install the embedder's weak-semantics pass. The VM installs it once
    /// when it takes ownership of the heap.
    pub fn set_post_mark_processor(&mut self, processor: PostMarkProcessor) {
        self.post_mark = Some(processor);
    }

    /// Run the installed weak-semantics pass after [`Self::mark_phase`] and
    /// before [`Self::sweep_phase`]. [`Self::collect_full`] calls it itself.
    pub fn run_post_mark_processing(&mut self) {
        self.with_gc_pause(GcPauseKind::SplitWeak, GcPauseTrigger::Explicit, |heap| {
            heap.run_post_mark_processing_work();
            ((), GcPauseOutcome::Completed)
        });
    }

    fn run_post_mark_processing_work(&mut self) {
        if let Some(processor) = self.post_mark {
            processor(self);
        }
        // Owner tables forget unreached cells last: the processor's
        // ephemeron fixpoint may still mark objects.
        let sources = self.extra_roots.clone();
        let mut seen: Vec<ExtraRoots> = Vec::with_capacity(sources.len());
        for source in sources {
            if seen.iter().any(|earlier| earlier.same_source(&source)) {
                continue;
            }
            source.sweep_weak(self);
            seen.push(source);
        }
    }

    /// Whether a collection since the last call may have emptied a
    /// finalization cell. Resets the flag.
    pub fn take_weak_cleanup_due(&mut self) -> bool {
        std::mem::take(&mut self.weak_cleanup_due)
    }

    /// Whether GC-stress mode is configured (env-enabled), regardless of
    /// whether it is armed yet.
    pub fn gc_stress_enabled(&self) -> bool {
        self.gc_stress_stride != 0
    }

    /// Configure GC stress on this heap: a scavenge (and, with `full`, a
    /// major collection) every `stride` allocations; `0` disables it. The
    /// in-process counterpart of `OTTER_GC_STRESS`, for tests that drive one
    /// heap without touching the process environment.
    pub fn set_gc_stress(&mut self, stride: u32, full: bool) {
        self.retire_machine_labs();
        self.gc_stress_stride = stride;
        self.gc_stress_full = full;
        self.gc_stress_counter = 0;
        self.gc_stress_armed = stride != 0;
    }

    /// Growth-ratio major-GC trigger for cap-less heaps.
    ///
    /// Called from the allocation slow path when no hard cap is set.
    /// Runs a full GC once old+LOS occupancy reaches the recomputed
    /// growth threshold (or the cage soft-cap backstop), then sets the
    /// next threshold from the post-GC live size. This is what keeps a
    /// cap-less heap from growing old space without bound — without it,
    /// `collect_full` is only ever reached via the cap path. No-op when
    /// a hard cap is configured (that path drives full GCs itself) or
    /// while a major GC is already in flight.
    fn maybe_major_gc(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        if self.always_allocate_depth != 0 || self.in_major_gc {
            return Ok(());
        }
        if self.sweeping.is_some() {
            // The last cycle's sweep is still settling the old generation's
            // size; no cycle starts before it completes.
            self.drain_old_sweep();
            if self.sweeping.is_some() {
                return Ok(());
            }
        }
        if self.marking.is_concurrent() {
            // Helpers are marking: hand them what the barrier shaded, and
            // finish the cycle once they ran dry, the old generation
            // overshot its limit by the margin the cycle may run into, or
            // external bytes reached theirs.
            self.marking.publish();
            if !self.marking.concurrent_done()
                && !self.limit_overshot_by_large_margin()
                && !self.external_limit_reached()
            {
                return Ok(());
            }
        } else {
            // The old-generation limit is soft: reaching it starts a
            // concurrent cycle, and only an overshoot past the margin stops
            // the world for one. The external limit stays hard.
            let due = self.major_gc_due();
            if (due || self.concurrent_marking_due())
                && !self.external_limit_reached()
                && self.concurrent_marking_possible()
                && !self.limit_overshot_by_large_margin()
            {
                self.with_gc_pause_result(
                    GcPauseKind::MarkStart,
                    GcPauseTrigger::GrowthBudget,
                    |heap| heap.start_concurrent_marking_work(external_visit),
                )?;
                return Ok(());
            }
            if !due {
                return Ok(());
            }
        }
        self.with_gc_pause_result(GcPauseKind::Full, GcPauseTrigger::GrowthBudget, |heap| {
            heap.collect_major_work(external_visit)
        })
    }

    /// Whether a concurrent marking cycle should start now: the old
    /// generation reached its start occupancy and a cycle can start.
    fn concurrent_marking_due(&mut self) -> bool {
        self.old_generation_consumed_bytes() >= self.concurrent_marking_start
            && self.concurrent_marking_possible()
    }

    /// Whether a concurrent marking cycle can start: none runs and helper
    /// threads exist.
    fn concurrent_marking_possible(&mut self) -> bool {
        !self.marking.is_marking()
            && self.machine_allocation_allowed()
            && self.gc_threads.workers() > 1
    }

    /// Whether the old generation passed its limit by more than a running
    /// concurrent cycle may let it: half the limit, at least
    /// [`LIMIT_OVERSHOOT_MARGIN_FOR_SMALL_HEAPS`], and at most half the room
    /// left under the cage ceiling.
    fn limit_overshot_by_large_margin(&self) -> bool {
        let consumed = self.old_generation_consumed_bytes();
        let limit = self.old_generation_limit;
        if consumed < limit {
            return false;
        }
        let ceiling = allocation_limits::cage_hardcap_bytes();
        let margin = (limit / 2)
            .max(LIMIT_OVERSHOOT_MARGIN_FOR_SMALL_HEAPS)
            .min(ceiling.saturating_sub(limit) / 2);
        consumed - limit >= margin
    }

    /// Whether external bytes reached their limit, which a mark-sweep
    /// answers at once.
    fn external_limit_reached(&self) -> bool {
        self.reserved_bytes >= self.external_limit
    }

    /// Begin a concurrent marking cycle: shade the roots and hand the drain
    /// to the helper threads, which run it beside the mutator until the
    /// collection that finishes the cycle stops them.
    fn start_concurrent_marking_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        // Much of the old generation may be reachable only through young
        // cells, which the concurrent marker never enters: a scavenge leaves
        // the nursery holding only live ones, whose old targets are roots.
        // Old cells are born black through the canonical allocator while the
        // cycle runs, so the old buffer stays retired.
        self.collect_minor_internal(external_visit)?;
        self.prepare_mark_cycle();
        self.marking.start_cycle();
        self.shade_young_edges();
        self.shade_roots(external_visit);
        // SAFETY: the cycle is active, the worklist holds gray old headers,
        // and every collection, heap walk and the heap's drop stop the
        // drain first.
        unsafe {
            self.marking
                .start_concurrent(&self.trace_table, &mut self.gc_threads)
        };
        self.marking.set_mutator_phase(true);
        Ok(())
    }

    fn collect_major_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        if self.always_allocate_depth != 0 || !(self.major_gc_due() || self.marking.is_concurrent())
        {
            return Ok(());
        }
        self.in_major_gc = true;
        let collect_result = self.collect_full_observed(
            external_visit,
            GcPauseTrigger::GrowthBudget,
            FullGcGoal::Throughput,
        );
        self.in_major_gc = false;
        collect_result
    }

    fn major_gc_due(&self) -> bool {
        // Runs regardless of any byte cap: the default cap equals the
        // cage size, so it can never fire before the cage physically
        // exhausts (pages carry overhead the byte-cap doesn't see).
        // This growth trigger is what actually bounds old-space.
        if self.in_major_gc {
            return false;
        }
        // The bytes old cells occupy, not the pages they span: swept holes
        // are reusable space, so a fragmented generation is measured by
        // what it holds. The limit is itself clamped below the cage ceiling,
        // so a large live set settles just under it instead of collecting
        // on every allocation.
        self.old_generation_consumed_bytes() >= self.old_generation_limit
            || self.reserved_bytes >= self.external_limit
    }

    /// Bytes the old generation's cells occupy: old-space cells, live or not
    /// yet swept, plus whole large-object pages. O(1).
    fn old_generation_consumed_bytes(&self) -> u64 {
        (self.old_space.allocated_bytes() as u64).saturating_add(
            (self.large_space.page_count() as u64).saturating_mul(crate::page::PAGE_SIZE as u64),
        )
    }

    /// Old/large page bytes: the old generation's occupancy.
    fn old_generation_page_bytes(&self) -> u64 {
        (self.old_space.page_count() as u64)
            .saturating_add(self.large_space.page_count() as u64)
            .saturating_mul(crate::page::PAGE_SIZE as u64)
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
    pub fn mark_phase(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.with_gc_pause_result(GcPauseKind::SplitMark, GcPauseTrigger::Explicit, |heap| {
            heap.mark_phase_work(external_visit)
        })
    }

    fn mark_phase_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.start_incremental_mark_phase(external_visit)?;
        // The whole mark runs in this pause: no mutator runs inside it.
        self.marking.set_mutator_phase(false);
        // SAFETY: STW pause; all pushed headers alive.
        unsafe { self.drain_marking_stopped() };
        Ok(())
    }

    /// Drain the marking worklist under a stop-the-world pause, on the
    /// helper threads as well once the old generation is large enough to
    /// share.
    ///
    /// # Safety
    ///
    /// STW pause; every header on the worklist is alive.
    unsafe fn drain_marking_stopped(&mut self) {
        // SAFETY: forwarded from the caller; trace callbacks are
        // object-local.
        unsafe {
            if self.old_generation_page_bytes() >= PARALLEL_MARK_MIN_OLD_BYTES {
                self.marking
                    .drain_parallel(&self.trace_table, &mut self.gc_threads);
            } else {
                self.marking.drain_full(&self.trace_table);
            }
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
    /// Uses a Dijkstra-style insertion barrier with a black-on-allocation
    /// new-object policy so the mutator can run between mark steps without
    /// losing a freshly published edge.
    pub fn start_incremental_mark_phase(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.with_gc_pause_result(GcPauseKind::SplitMark, GcPauseTrigger::Explicit, |heap| {
            heap.start_incremental_mark_phase_work(external_visit)
        })
    }

    fn start_incremental_mark_phase_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        // Scavenge first: the nursery then holds only survivors, which mark
        // the old generation as roots.
        self.collect_minor_internal(external_visit)?;
        self.prepare_mark_cycle();
        self.marking.start_cycle();
        self.shade_young_edges();
        self.shade_roots(external_visit);
        self.marking.set_mutator_phase(true);
        Ok(())
    }

    /// Reset the old generation's live counters and make every old cell
    /// white for a new marking cycle.
    fn prepare_mark_cycle(&mut self) {
        self.old_space.reset_live_bytes();
        self.large_space.reset_live_bytes();
        // Every header must be white before we begin. The previous cycle's
        // sweep whitened its survivors and nothing shades outside a cycle,
        // so the whole-heap reset runs only after an unswept cycle or a
        // restored image.
        if !self.marks_white {
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
            }
        }
        self.marks_white = false;
    }

    /// Shade every old cell a young cell points at: roots of the old
    /// generation's marking, which never enters young cells. Right after a
    /// scavenge the nursery holds only the cells it kept alive.
    fn shade_young_edges(&mut self) {
        let marking: *mut MarkingState = &mut self.marking;
        let trace_table = &self.trace_table;
        for page in self.new_space.from_pages() {
            // SAFETY: STW pause with the nursery buffer published; every
            // from-space cell is initialized, with a registered type.
            unsafe {
                page.for_each_object(|h, _| {
                    if (*h).type_tag() != crate::header::FREE_TAG {
                        trace_table.trace(h, &mut |slot| (*marking).shade_from_slot(slot));
                    }
                });
            }
        }
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
        self.with_gc_pause(GcPauseKind::SplitStep, GcPauseTrigger::Explicit, |heap| {
            (
                heap.incremental_mark_step_work(budget),
                GcPauseOutcome::Completed,
            )
        })
    }

    fn incremental_mark_step_work(&mut self, budget: usize) -> usize {
        if !self.marking.is_marking() {
            return 0;
        }
        self.prepare_collection_observations();
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
    ///
    /// # Errors
    ///
    /// Same as the scavenge it begins with.
    pub fn finish_incremental_mark_phase(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.with_gc_pause_result(GcPauseKind::SplitMark, GcPauseTrigger::Explicit, |heap| {
            heap.finish_incremental_mark_phase_work(external_visit)
        })
    }

    fn finish_incremental_mark_phase_work(
        &mut self,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<(), OutOfMemory> {
        self.marking.set_mutator_phase(false);
        self.marking.stop_concurrent(&mut self.gc_threads);
        // The scavenge grays what it promotes; the nursery then holds only
        // survivors, which are roots of the old generation.
        self.collect_minor_internal(external_visit)?;
        self.shade_young_edges();
        self.prepare_collection_observations();
        // Re-shade roots — the mutator may have rewritten handles
        // or globals between mark steps. The barrier covered every
        // *new* white-child publication, but a slot whose pointer
        // changed twice (new white → another white) only retains
        // its final value, so we walk the live root set again to
        // pick up any white target the barrier already shaded gray
        // (idempotent) plus any pointer the mutator stored straight
        // into a root slot (where no barrier fires).
        self.shade_roots(external_visit);
        self.marking.take_final_visits();
        // SAFETY: STW pause covers the final drain.
        unsafe { self.drain_marking_stopped() };
        if crate::scavenger::gc_verify_enabled() {
            self.verify_marking_closed();
        }
        Ok(())
    }

    /// `OTTER_GC_VERIFY`: after the final drain, every marked old cell's old
    /// children are marked. A miss names a store no barrier saw.
    fn verify_marking_closed(&self) {
        let trace_table = &self.trace_table;
        let check = |page: &crate::page::Page| {
            // SAFETY: STW pause; every non-filler cell is a registered body.
            unsafe {
                page.for_each_object(|h, _| {
                    if (*h).type_tag() == crate::header::FREE_TAG || !(*h).is_marked() {
                        return;
                    }
                    trace_table.trace(h, &mut |slot| {
                        let raw = (*slot).0;
                        if raw == 0 {
                            return;
                        }
                        let child = cage_base().add(raw as usize) as *const GcHeader;
                        if !(*child).is_young() && !(*child).is_marked() {
                            panic!(
                                "OTTER_GC_VERIFY: marked cell {:#x} (tag {:#x}) points at unmarked \
                                 old cell {raw:#x} (tag {:#x})",
                                h as usize - cage_base() as usize,
                                (*h).type_tag(),
                                (*child).type_tag(),
                            );
                        }
                    });
                });
            }
        };
        self.old_space.pages().iter().for_each(check);
        self.large_space.pages().iter().for_each(check);
        self.young_large_space.pages().iter().for_each(check);
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
        self.trace_embedder_roots(&mut shade);
        if let Some(data) = &self.isolate_data {
            data.visit_old_roots(&mut shade);
        }
        external_visit(&mut shade);
        visit_extra_roots_deduped(&self.extra_roots, RootSet::All, &mut shade);
        self.frame_root_providers.trace(&mut shade);
    }

    /// Mark additional raw objects during an active mark phase and
    /// drain their transitive closure.
    ///
    /// Returns `true` when at least one previously-white object was
    /// discovered.
    #[doc(hidden)]
    pub fn mark_additional(&mut self, additions: impl IntoIterator<Item = RawGc>) -> bool {
        self.with_gc_pause(GcPauseKind::SplitMark, GcPauseTrigger::Explicit, |heap| {
            (
                heap.mark_additional_work(additions),
                GcPauseOutcome::Completed,
            )
        })
    }

    fn mark_additional_work(&mut self, additions: impl IntoIterator<Item = RawGc>) -> bool {
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
        raw_is_marked(raw)
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
        self.ephemerons.retain_marked(raw_is_marked);
    }

    /// Prune weak-reference/finalization registries to handles
    /// that survived the current mark phase. Must run before sweep
    /// frees dead bodies.
    pub fn prune_weak_finalization_registry_to_marked(&mut self) {
        self.weak_finalization.retain_marked(raw_is_marked);
    }

    /// Finish an explicit split full-GC cycle and release unused physical payload.
    pub fn sweep_phase(&mut self) {
        self.with_gc_pause(GcPauseKind::SplitSweep, GcPauseTrigger::Explicit, |heap| {
            heap.sweep_phase_with_pause_start(Instant::now(), FullGcGoal::ReleaseMemory, None);
            ((), GcPauseOutcome::Completed)
        });
    }

    /// Sweep after a completed mark. With `limits` (the old generation's
    /// size before the collection), the allocation limits follow the
    /// completed sweep.
    fn sweep_phase_with_pause_start(
        &mut self,
        pause_start: Instant,
        goal: FullGcGoal,
        limits: Option<u64>,
    ) {
        self.retire_machine_labs();
        self.prepare_collection_observations();
        // Heap-owned weak state is always present, even when a split sweep
        // has no temporary mutator/root-source registration. Mark bits are
        // final and no old-space cell has been reclaimed yet.
        if let Some(data) = &self.isolate_data {
            data.sweep_weak(self);
        }
        // This is also the split mark/sweep entry. Resolve observations here
        // so even a caller without an embedder post-mark pass cannot leave a
        // freed old offset in an owner cache. Full collections have already
        // completed the ephemeron fixpoint before reaching this boundary.
        for (index, source) in self.extra_roots.iter().enumerate() {
            if !self.extra_roots[..index]
                .iter()
                .any(|prior| prior.same_source(source))
            {
                source.process_weak(&mut |raw| raw_is_marked(raw).then_some(raw));
            }
        }
        self.prune_ephemeron_registry_to_marked();
        self.prune_weak_finalization_registry_to_marked();
        self.weak_cleanup_due |= self.weak_finalization.has_finalization_registries();

        // White large cells are reclaimed and wholly dead large pages reaped
        // here; old pages are swept by an [`sweep::OldSweep`] cycle, in this
        // pause or behind it. The finishing scavenge has already copied
        // nursery survivors into the active from-space. Preserve its
        // initialized prefix, including cells whose old parent became
        // unreachable only in this full mark; the next scavenge can reclaim
        // those cells. Only inactive payload and unused active tails may be
        // discarded.
        //
        // The same walks accumulate per-tag live counts for
        // [`crate::stats::GcStats`] reconciliation, so a full collection
        // pays for one heap walk.
        let track_cap = self.max_heap_bytes != 0;
        let mut totals = sweep::SweepTotals::default();
        // Prune the remembered set to parents that survived this full GC.
        // The embedded pre-mark scavenge (`start_incremental_mark_phase`)
        // repopulated the buffer with the live old→young edges among
        // survivors, but mark-sweep may then free some of those parents as
        // unreachable. A dead parent left in the buffer would dangle once
        // `reap_dead_pages` frees its page and the cage offset is reused, so
        // drop every entry whose header did not survive the mark (and clear
        // its now-meaningless remembered bit). Runs before the sweep below
        // whitens the survivors, while every header still shows its color.
        self.remembered_parents.retain(|raw| {
            // SAFETY: STW pause; `raw` is an in-cage old/large parent offset
            // recorded by the barrier; its header is live until the reap below.
            let header = unsafe { cage_base().add(raw.0 as usize) as *const GcHeader };
            if unsafe { (*header).is_marked() } {
                true
            } else {
                unsafe { (*header).clear_remembered() };
                false
            }
        });
        // SAFETY: STW pause.
        unsafe {
            for page in self.large_space.pages() {
                page.for_each_object(|h, _| {
                    let tag = (*h).type_tag() as usize;
                    let size = (*h).size_bytes() as usize;
                    if !(*h).is_marked() {
                        // Old/large space is non-moving and reaps only
                        // whole dead pages, so a dropped corpse stays in
                        // the bump range and is re-walked by later
                        // sweeps. Drop exactly once — a second
                        // `drop_in_place` would double-free any buffer
                        // the payload owns (e.g. a string `Vec<u16>`).
                        if !(*h).is_swept() {
                            let tag_u8 = (*h).type_tag();
                            if let Some(finalize_fn) = self.trace_table.get_finalize(tag_u8) {
                                finalize_fn(h);
                            }
                            if let Some(drop_fn) = self.trace_table.get_drop(tag_u8) {
                                drop_fn(h);
                            }
                            (*h).set_swept();
                            totals.reclaimed += size;
                        }
                    } else {
                        (*h).clear_mark();
                        totals.live_count[tag] = totals.live_count[tag].wrapping_add(1);
                        totals.live_bytes[tag] = totals.live_bytes[tag].wrapping_add(size);
                    }
                });
                if track_cap && page.header().live_bytes != 0 {
                    totals.retained = totals
                        .retained
                        .saturating_add(page.header().allocated_bytes as u64);
                }
            }
            // Young from-space is recycled at next scavenge but
            // any survivor still in to-space (after the
            // pre-sweep scavenge flip) belongs to the live set.
            // After the flip, those survivors live in
            // `from_pages()` of the new orientation.
            for page in self.new_space.from_pages() {
                // These physical survivor cells remain in the nursery until
                // the next scavenge, regardless of per-type mark statistics.
                if track_cap {
                    totals.retained = totals
                        .retained
                        .saturating_add(page.header().allocated_bytes as u64);
                }
                // Every nursery cell is a survivor of the finishing scavenge.
                page.for_each_object(|h, _| {
                    if (*h).type_tag() != crate::header::FREE_TAG {
                        let tag = (*h).type_tag() as usize;
                        let size = (*h).size_bytes() as usize;
                        totals.live_count[tag] = totals.live_count[tag].wrapping_add(1);
                        totals.live_bytes[tag] = totals.live_bytes[tag].wrapping_add(size);
                    }
                });
            }
        }
        let _ = self.large_space.reap_dead_pages();
        self.marking.finish_cycle();
        self.gc_stats.gc_cycles = self.gc_stats.gc_cycles.saturating_add(1);
        let elapsed = pause_start.elapsed();
        self.gc_stats.last_gc_pause_ms = elapsed.as_secs_f32() * 1000.0;
        self.gc_stats.full_pause_ns_total = self
            .gc_stats
            .full_pause_ns_total
            .saturating_add(elapsed.as_nanos().min(u128::from(u64::MAX)) as u64);
        self.start_old_sweep(totals, goal, pause_start, limits);
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
    fn generated_bytes(&self) -> u64 {
        let live = if self.lab_page == 0 {
            0
        } else {
            (self.lab.top - self.lab_start) as u64
        };
        (self.lab_consumed_bytes + live).saturating_sub(self.runtime_lab_bytes)
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
        debug_assert!(self.sweeping.is_none(), "finish the sweep before walking");
        self.sync_lab();
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
            for page in self.young_large_space.pages() {
                page.for_each_object(|h, _| visitor(h));
            }
        }
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

    /// Old-space pages currently owned.
    #[must_use]
    pub fn old_space_page_count(&self) -> usize {
        self.old_space.page_count()
    }

    /// Take ownership of a page already filled by
    /// [`crate::heap_image::GcHeap::restore_old_space`].
    ///
    /// The page's objects are live and relocated; the sweeper treats it
    /// like any other old page from here.
    pub(crate) fn adopt_restored_old_page(&mut self, page: crate::page::Page) {
        self.old_space.adopt_page(page);
        self.marks_white = false;
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

    /// Page lists for every space, in census order: old, young
    /// from-space, large, young large. Lets [`crate::census`] attribute
    /// objects to a space without opening up the space fields.
    pub(crate) fn census_spaces(&self) -> [(crate::page::SpaceKind, &[crate::page::Page]); 4] {
        debug_assert!(self.sweeping.is_none(), "finish the sweep before walking");
        self.sync_lab();
        [
            (crate::page::SpaceKind::Old, self.old_space.pages()),
            (crate::page::SpaceKind::NewFrom, self.new_space.from_pages()),
            (crate::page::SpaceKind::Large, self.large_space.pages()),
            (
                crate::page::SpaceKind::NewLarge,
                self.young_large_space.pages(),
            ),
        ]
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

/// Visit every registered extra-roots entry, skipping later entries
/// that dispatch to the same source as an earlier one. Re-entrant VM
/// scopes re-register the same interpreter; tracing it once per pause
/// keeps the root walk linear in distinct sources. Visiting is
/// idempotent, so a false-negative match only costs a redundant walk.
fn visit_extra_roots_deduped(
    entries: &[ExtraRoots],
    roots: RootSet,
    visitor: &mut dyn FnMut(*mut RawGc),
) {
    for (i, entry) in entries.iter().enumerate() {
        if entries[..i].iter().any(|prev| prev.same_source(entry)) {
            continue;
        }
        entry.visit(roots, visitor);
    }
}

/// Read the payload of the live cell `handle`. The cage alone addresses it:
/// no heap state is read, so a thread reading the heap while no collection
/// runs reads what the heap would.
#[inline]
pub fn read_payload<T: Traceable, R>(handle: Gc<T>, f: impl FnOnce(&T) -> R) -> R {
    debug_assert!(!handle.is_null(), "read_payload on null handle");
    debug_assert_handle_live::<T>(handle, "read_payload");
    // SAFETY: handle was issued by `alloc` / `alloc_old`; the payload lives
    // immediately after the header at the same allocation.
    unsafe {
        let payload =
            (handle.as_header_ptr() as *mut u8).add(std::mem::size_of::<GcHeader>()) as *const T;
        f(&*payload)
    }
}

impl std::fmt::Debug for GcHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcHeap")
            .field("new_pages", &self.new_space.from_page_count())
            .field("old_pages", &self.old_space.page_count())
            .field("large_pages", &self.large_space.page_count())
            .field("young_large_pages", &self.young_large_space.page_count())
            .field("handles", &self.handle_stack.len())
            .field("is_marking", &self.marking.is_marking())
            .finish()
    }
}

impl Drop for GcHeap {
    fn drop(&mut self) {
        self.marking.stop_concurrent(&mut self.gc_threads);
        self.complete_old_sweep();
        self.retire_machine_labs();
        // Runtime disposal is a terminal STW boundary. Page ownership alone
        // returns cage memory, but Rust payloads may own external resources
        // (host data, strings, buffers, reservation tokens) that must be
        // finalized and dropped before those pages disappear.
        let dispose_page = |page: &crate::page::Page| {
            // SAFETY: the heap has exclusive ownership during Drop; every
            // non-filler, non-swept header precedes one live registered body.
            unsafe {
                page.for_each_object(|header, _| {
                    let tag = (*header).type_tag();
                    if tag == crate::header::FREE_TAG
                        || (*header).is_swept()
                        || (*header).is_forwarded()
                    {
                        return;
                    }
                    if let Some(finalize) = self.trace_table.get_finalize(tag) {
                        finalize(header);
                    }
                    if let Some(drop_fn) = self.trace_table.get_drop(tag) {
                        drop_fn(header);
                    }
                    (*header).set_swept();
                });
            }
        };

        for page in self.new_space.from_pages() {
            dispose_page(page);
        }
        for page in self.old_space.pages() {
            dispose_page(page);
        }
        for page in self.large_space.pages() {
            dispose_page(page);
        }
        for page in self.young_large_space.pages() {
            dispose_page(page);
        }
    }
}

/// Header-only mark check for registry pruning: reads the mark bit
/// straight off the object header without touching heap state.
/// Whether `raw` survives the old generation's marking: marked, or young,
/// since a finishing collection's nursery holds only scavenge survivors.
fn raw_is_marked(raw: RawGc) -> bool {
    if raw.is_null() {
        return false;
    }
    // SAFETY: registries only hold heap-issued raw handles.
    unsafe {
        let header = &*raw.as_header_ptr();
        header.is_young() || header.is_marked()
    }
}

#[path = "heap/tenured.rs"]
mod tenured;

#[path = "heap/sweep.rs"]
mod sweep;
mod sweeping;
pub use tenured::{OLD_LAB_BYTES, SitePlacement};

#[cfg(test)]
#[path = "heap/allocation_group_tests.rs"]
mod allocation_group_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::OpaqueLeaf;

    #[test]
    fn no_collect_allocation_returns_payload_on_cap_miss() {
        let mut heap = GcHeap::with_max_heap_bytes(1).expect("heap");
        let leaf = OpaqueLeaf { payload: 42 };

        match heap.try_alloc_no_collect_or_return(leaf) {
            Ok(_) => panic!("allocation should not fit under the cap"),
            Err(returned) => assert_eq!(returned.payload, 42),
        }
    }

    #[test]
    fn always_allocate_scope_never_collects() {
        let mut heap = GcHeap::new().expect("heap");
        heap.gc_stress_stride = 1;
        heap.gc_stress_armed = true;
        let before = (heap.gc_stats().minor_gc_cycles, heap.gc_stats().gc_cycles);
        let mut noop = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        {
            let _scope = heap.always_allocate_scope();
            // Stride 1 would collect on every allocation, and this many
            // leaves overflow the default nursery; neither may collect here.
            for payload in 0..300_000 {
                heap.alloc_with_roots(OpaqueLeaf { payload }, &mut noop)
                    .expect("allocation inside the scope");
            }
            assert_eq!(
                (heap.gc_stats().minor_gc_cycles, heap.gc_stats().gc_cycles),
                before
            );
        }
        heap.alloc_with_roots(OpaqueLeaf { payload: 0 }, &mut noop)
            .expect("allocation after the scope");
        assert!(heap.gc_stats().minor_gc_cycles > before.0);
    }

    #[test]
    fn no_collect_allocation_still_succeeds_on_fast_path() {
        let mut heap = GcHeap::new().expect("heap");
        let handle = heap
            .try_alloc_no_collect_or_return(OpaqueLeaf { payload: 7 })
            .expect("fast allocation");

        heap.read_payload(handle, |body| assert_eq!(body.payload, 7));
    }
}

#[cfg(test)]
mod reservation_tests;

#[cfg(test)]
mod cap_admission_tests;

#[cfg(test)]
mod collection_accounting_tests;

#[cfg(test)]
#[path = "heap/nursery_discard_tests.rs"]
mod nursery_discard_tests;

#[cfg(test)]
#[path = "heap/old_space_discard_tests.rs"]
mod old_space_discard_tests;
