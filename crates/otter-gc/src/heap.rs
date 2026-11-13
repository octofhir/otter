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
//! - Optional remembered-set verification uses integer address indices (see
//!   [`verification`]) and rejects misses before nursery movement.
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
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use crate::compressed::{Cage, Gc, RawGc};
use crate::ephemeron::EphemeronRegistry;
use crate::external::SharedExternalState;
use crate::extra_roots::ExtraRoots;
use crate::finalize::WeakFinalizationRegistry;
use crate::frame_roots::FrameRootProviders;
use crate::handle::{GlobalHandleTable, HandleStack};
use crate::header::GcHeader;
use crate::lab::LinearAllocationArea;
use crate::marking::MarkingState;

use crate::observation::GcPauseRecorder;
use crate::oom::OutOfMemory;
use crate::scavenger::ScavengeStats;
use crate::space::{LargeObjectSpace, NewSpace, OldSpace};
use crate::stats::GcStats;
use crate::trace::{TraceTable, Traceable};

#[path = "heap/accounting.rs"]
mod accounting;
#[path = "heap/allocation_limits.rs"]
mod allocation_limits;
#[path = "heap/inspection.rs"]
mod inspection;
#[path = "heap/machine.rs"]
mod machine;
#[path = "heap/major.rs"]
mod major;
#[path = "heap/minor.rs"]
mod minor;
#[path = "heap/mutation.rs"]
mod mutation;
#[path = "heap/nursery.rs"]
mod nursery;
#[path = "heap/observation.rs"]
mod observation;
#[path = "heap/old_allocation.rs"]
mod old_allocation;
#[path = "heap/registration.rs"]
mod registration;
#[path = "heap/roots.rs"]
mod roots;
#[path = "heap/verification.rs"]
mod verification;
use allocation_limits::{
    EXTERNAL_ALLOCATION_SOFT_LIMIT, INITIAL_OLD_GENERATION_SIZE, cage_hardcap_bytes,
    concurrent_marking_start,
};
use verification::debug_assert_handle_live;

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
#[path = "heap/verification_tests.rs"]
mod verification_tests;

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
