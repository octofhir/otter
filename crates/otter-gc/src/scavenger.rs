//! Parallel young-generation scavenger.
//!
//! A copying collector over the semispace nursery. Roots and the remembered
//! old parents point into from-space; each from-space target is copied to
//! to-space (or promoted to old space) once, its slot rewritten to the copy,
//! and the copy traced in turn until no from-space target remains reachable.
//! The tracing after the roots runs on the heap's helper threads together
//! with the collecting thread.
//!
//! # Algorithm
//!
//! 1. The collecting thread evacuates every root slot's target. Root
//!    visitors run exactly as they always did, one slot at a time.
//! 2. Every worker — the collecting thread and each helper that joins while
//!    it still works — claims chunks of the remembered old parents (the
//!    old/large objects the write barrier recorded as holding an old→young
//!    edge) and re-traces them, then drains worklists of freshly copied
//!    bodies, tracing each and evacuating its from-space children. A worker
//!    with a long worklist hands half to idle workers; the phase ends when
//!    every joined worker is idle and no work is shared, without waiting for
//!    a helper that has not started.
//! 3. The collecting thread runs the ephemeron fixpoint and rewrites weak
//!    registry slots and owner-held weak observations.
//! 4. Finalize and drop the unforwarded bodies the new space tracks for
//!    reclamation; untracked dead cells are never visited.
//! 5. Set every to-space page's `age_mark` to its bump top: everything
//!    copied there survived this scavenge.
//! 6. Flip from↔to. Survivor pages become the new from-space; the
//!    mutator allocates above their age marks. Prior from-space becomes
//!    the new to-space, whose evacuated/dead input storage is recycled.
//!
//! # Evacuation
//!
//! A worker claims a from-space cell by setting its forwarded and copying
//! flag bits in one compare-and-swap, copies it, writes the forwarding offset
//! and clears the copying bit with release ordering; a worker that loses the
//! claim waits for that and rewrites its slot to the published copy. Each
//! worker copies into to-space pages it claims by index and promotes small
//! cells into old-space linear areas of its own; larger promotions and
//! area refills take the old space's lock. A copied body enters exactly one
//! worklist, so every body is traced by exactly one worker.
//!
//! # Allocation-site feedback
//!
//! Evacuating a cell tagged with an allocation site counts it as found for
//! that site and clears the copy's tag, so a cell is counted once in its life.
//! The heap digests the counts into pretenuring decisions after the flip.
//!
//! # Promotion
//!
//! An object is copied to to-space on its first survival and promoted to
//! old space on its second (the semispace age mark). The to-space pages
//! that received survivors become the next from-space and the mutator keeps
//! bump-allocating above their `age_mark`, so a fresh object never inherits
//! its page's age. Children of remembered old parents and of freshly promoted
//! bodies are promoted at once.
//!
//! A young large cell is never copied. The first visit to reach it clears
//! its young bit in one atomic step and queues it like a promoted copy; the
//! heap then moves its region to the old large object space and releases
//! every young large region no visit reached.
//!
//! # Contents
//!
//! - [`ScavengeStats`] — counters returned by `scavenge`.
//! - [`scavenge`] — the entry point.
//!
//! # Invariants
//!
//! - Roots and newly copied bodies receive complete strong-slot walks.
//! - Remembered old parents may use type-owned mutation ranges, but every
//!   remaining young edge is retained for the following collection.
//! - Promotion space is admitted before the first forwarding write. A failed
//!   preflight makes no forwarding write and publishes no reclamation commit.
//!   The reservation covers every worker's partly used page and linear area.
//! - Exact aligned nursery input equals copied + promoted + reclaimed bytes.
//!   The input byte total uses maintained physical allocation counters; the
//!   promotion page reservation is a separate capacity upper bound.
//! - Slots are read and written with relaxed atomic accesses: a root slot may
//!   be visited twice, and both visits write the same copy.
//! - Trace callbacks are object-local: a worker tracing one body touches only
//!   that body's slots and the headers of their targets.
//!
//! # See also
//!
//! - [`crate::barrier`] — the generational barrier that feeds the
//!   object-granular remembered set this scavenger drains.
//! - [`crate::gc_threads`] — the helper threads.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::compressed::{RawGc, cage_base};
use crate::gc_threads::{GcThreads, WorkPool};
use crate::header::{ForwardingClaim, GcHeader};
use crate::heap::RootSlotVisitor;
use crate::oom::OutOfMemory;
use crate::page::{CELL_SIZE, PAGE_HEADER_SIZE, Page, SpaceKind, align_up};
use crate::space::{NewSpace, OldSpace};
use crate::trace::TraceTable;

/// Stats returned by [`scavenge`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScavengeStats {
    /// Physical aligned cell bytes copied to to-space (not promoted).
    pub copied_bytes: usize,
    /// Physical aligned cell bytes promoted to old-space.
    pub promoted_bytes: usize,
    /// Young large cells promoted in place: their pages join the old large
    /// object space.
    pub promoted_large_bytes: usize,
    /// Physical nursery bytes reclaimed after successful evacuation. The
    /// preflight already totals exact allocated input bytes; copied/promoted
    /// cells retain their aligned header sizes and are deducted once.
    pub reclaimed_bytes: usize,
    /// Slot updates performed.
    pub slot_updates: usize,
    /// Minor-GC pause time in nanoseconds. Populated by the heap
    /// caller around the [`scavenge`] call, not by the scavenge
    /// itself (the pause spans more than the inner work).
    pub minor_pause_ns: u64,
    /// Root slots visited, excluding remembered-parent and copied-body slots.
    pub root_slots_scanned: usize,
    /// Remembered-set entries scanned this scavenge — the count of old/large
    /// parents recorded by the barrier as holding an old→young edge.
    pub dirty_cards_scanned: usize,
    /// Old-space object headers strided to re-derive which object owns an
    /// edge. Holding the parent objects directly keeps this at zero; the
    /// counter exists to prove no per-page header walk remains.
    pub old_headers_walked: usize,
    /// Remembered parents traced to evacuate their young children.
    pub objects_retraced: usize,
    /// Slots visited while tracing remembered parents and freshly promoted bodies.
    pub slots_scanned: usize,
    /// Threads that traced this scavenge, the collecting thread included.
    pub workers: usize,
}

impl ScavengeStats {
    fn absorb(&mut self, other: &Self) {
        self.copied_bytes += other.copied_bytes;
        self.promoted_bytes += other.promoted_bytes;
        self.promoted_large_bytes += other.promoted_large_bytes;
        self.slot_updates += other.slot_updates;
        self.root_slots_scanned += other.root_slots_scanned;
        self.dirty_cards_scanned += other.dirty_cards_scanned;
        self.old_headers_walked += other.old_headers_walked;
        self.objects_retraced += other.objects_retraced;
        self.slots_scanned += other.slots_scanned;
    }
}

/// Nursery input below which the collecting thread traces alone: waking the
/// helpers would cost more than the copying.
const PARALLEL_MIN_NURSERY_BYTES: usize = 1024 * 1024;
/// Promotions at most this large go through a worker's own linear area.
const LAB_OBJECT_MAX: usize = 2048;
/// Bytes one promotion linear area asks the old space for.
const LAB_BYTES: usize = 32 * 1024;
/// Worklist length past which a worker hands half to idle workers.
const SHARE_THRESHOLD: usize = 64;
/// Remembered parents one worker claims at a time.
const REMEMBERED_CHUNK: usize = 32;

/// A worklist entry: the cage offset of a copied body, with
/// [`PROMOTED_ENTRY`] set when it was promoted, so that its young children
/// are promoted too.
type Entry = u32;
const PROMOTED_ENTRY: u32 = 1;
const _: () = assert!(CELL_SIZE > 1);

/// The old space, reachable from every worker under one lock.
struct OldSpaceRef(*mut OldSpace);

/// State every worker of one scavenge shares.
struct Shared {
    cage: *mut u8,
    verify: bool,
    /// An old-generation marking cycle is active: promoted copies are born
    /// gray and listed for the marker.
    marking: bool,
    trace_table: *const TraceTable,
    to_pages: *const Page,
    to_page_count: usize,
    next_to_page: AtomicUsize,
    old_space: Mutex<OldSpaceRef>,
    remembered: Vec<RawGc>,
    next_remembered: AtomicUsize,
    sites: crate::pretenuring::FoundCounters,
    pool: WorkPool<Entry>,
    /// What each helper hands back when its part ends.
    finished: Mutex<Vec<WorkerOutput>>,
}

// SAFETY: the raw pointers name the heap's spaces and trace table for the
// duration of one scavenge, during which the mutator is stopped; mutable
// state behind them is reached only under `old_space`'s lock, through pages
// a single worker owns, or through atomics.
unsafe impl Sync for Shared {}

impl Shared {
    fn old_space(&self) -> MutexGuard<'_, OldSpaceRef> {
        self.old_space
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn remembered_exhausted(&self) -> bool {
        self.next_remembered.load(Ordering::Relaxed) >= self.remembered.len()
    }
}

/// What a worker leaves behind for the collecting thread to merge.
struct WorkerOutput {
    remembered: Vec<RawGc>,
    promoted_gray: Vec<RawGc>,
    stats: ScavengeStats,
}

/// One thread's part of a scavenge.
struct Worker<'s> {
    shared: &'s Shared,
    local: Vec<Entry>,
    /// Base of the to-space page this worker copies into; null for none.
    to_page: *mut u8,
    /// Old-space linear area `[cursor, end)` this worker promotes into.
    lab_cursor: u32,
    lab_end: u32,
    /// Old/large parents found still pointing young.
    remembered: Vec<RawGc>,
    /// Copies promoted gray during a marking cycle.
    promoted_gray: Vec<RawGc>,
    stats: ScavengeStats,
    /// True while a remembered parent or a freshly promoted body is traced:
    /// its young children promote immediately, and its slots count toward
    /// the remembered/promoted work counter. This policy does not select the
    /// trace callback: newly promoted bodies always receive a full walk.
    in_dirty_scan: bool,
    /// Whether the body being traced lives in old or large space, so a young
    /// child it keeps must enter the remembered set. Classified once per
    /// traced body rather than per slot.
    parent_old: bool,
}

/// Read one slot. A root slot may be visited by two workers.
#[inline(always)]
unsafe fn load_slot(slot: *mut RawGc) -> u32 {
    // SAFETY: caller passes a dereferenceable, 4-aligned slot.
    unsafe { AtomicU32::from_ptr(slot.cast::<u32>()).load(Ordering::Relaxed) }
}

/// Write one slot; see [`load_slot`].
#[inline(always)]
unsafe fn store_slot(slot: *mut RawGc, offset: u32) {
    // SAFETY: caller passes a dereferenceable, 4-aligned slot.
    unsafe { AtomicU32::from_ptr(slot.cast::<u32>()).store(offset, Ordering::Relaxed) }
}

/// Run a full scavenge.
///
/// `root_slots` is a list of `*mut RawGc` slot addresses. The
/// scavenger may rewrite each in place. `external_visit` is a
/// hook for additional root sources (handle stack, global
/// handles); each call yields the additional slots.
/// `ephemeron_registry_slots` and `weak_registry_slots` are non-root registry
/// entries. Ephemeron tables are scanned after ordinary strong reachability:
/// their keys are updated only if already live, and their values become strong
/// only for such live keys. Remaining weak registry slots are rewritten only
/// when their target was already forwarded; otherwise they are nulled for later
/// pruning. `threads` are the heap's helper threads. While an old-generation
/// marking cycle runs, `promoted_gray` receives every promoted copy, born
/// gray for the marker to visit.
/// `process_weak` resolves owner-held observations after the same ephemeron
/// fixpoint, before the source nursery can be recycled.
///
/// # Safety
///
/// - The mutator must be paused for the duration of the call.
/// - Every `*mut RawGc` from `root_slots` and `external_visit`
///   must address a valid slot inside the cage (or null).
/// - The trace table must register every type tag occurring in
///   from-space and old-space, and its trace callbacks must be object-local.
#[allow(clippy::too_many_arguments)]
pub unsafe fn scavenge(
    new_space: &mut NewSpace,
    old_space: &mut OldSpace,
    trace_table: &TraceTable,
    root_slots: &[*mut RawGc],
    external_visit: &mut RootSlotVisitor<'_>,
    ephemeron_registry_slots: &[*mut RawGc],
    weak_registry_slots: &[*mut RawGc],
    process_weak: &mut crate::heap::WeakRefProcessor<'_>,
    remembered_parents: &mut Vec<RawGc>,
    host_refs: &mut crate::host_refs::HostRefTable,
    sites: &mut crate::pretenuring::AllocationSites,
    threads: &mut GcThreads,
    mut promoted_gray: Option<&mut Vec<RawGc>>,
) -> Result<ScavengeStats, OutOfMemory> {
    let (active_pages, allocated_nursery_bytes) =
        new_space
            .from_pages()
            .iter()
            .fold((0usize, 0usize), |(pages, bytes), page| {
                let allocated = page.header().allocated_bytes;
                (pages + usize::from(allocated != 0), bytes + allocated)
            });
    let workers = if allocated_nursery_bytes >= PARALLEL_MIN_NURSERY_BYTES {
        threads.workers()
    } else {
        1
    };
    // Reserve every page promotion/fallback can need before forwarding the
    // first object. After a forwarding write, cage exhaustion cannot be
    // reported safely because roots and semispaces would already be partially
    // rewritten.
    //
    // Every nursery object is at most half a page. First-fit packing therefore
    // uses at most twice the number of source pages: at most one destination
    // page can finish half-empty. Pages selected by age (or all active pages
    // when a remembered parent can reach any nursery page) need that 2× bound
    // in old space. Non-promoted pages use to-space first; reserve only their
    // possible overflow beyond the existing to-space page count. Each worker
    // can leave one more to-space page and one linear area partly used.
    let promotion_pages = if remembered_parents.is_empty() {
        new_space
            .from_pages()
            .iter()
            .filter(|page| {
                page.header().allocated_bytes != 0 && page.header().age_mark > PAGE_HEADER_SIZE
            })
            .count()
    } else {
        active_pages
    };
    let copy_pages = active_pages.saturating_sub(promotion_pages);
    let copy_overflow = copy_pages
        .saturating_mul(2)
        .saturating_add(workers)
        .saturating_sub(new_space.to_pages().len());
    let reserve_count = promotion_pages
        .saturating_mul(2)
        .saturating_add(copy_overflow)
        .saturating_add(workers);
    old_space.reserve_promotion_pages(
        reserve_count,
        allocated_nursery_bytes.saturating_add(workers.saturating_mul(2 * LAB_BYTES)),
    )?;

    // Snapshot the remembered parents recorded by the mutator since the last
    // scavenge, then leave the live buffer empty so `remember` can re-fill it
    // with the parents that still hold an old→young edge after this scavenge
    // — the card model's "snapshot dirty cards, clear them, re-dirty
    // survivors", object-granular and find-cost-free.
    let snapshot_remembered: Vec<RawGc> = std::mem::take(remembered_parents);
    let shared = Shared {
        cage: cage_base(),
        verify: gc_verify_enabled(),
        marking: promoted_gray.is_some(),
        trace_table,
        to_pages: new_space.to_pages().as_ptr(),
        to_page_count: new_space.to_pages().len(),
        next_to_page: AtomicUsize::new(0),
        old_space: Mutex::new(OldSpaceRef(old_space)),
        remembered: snapshot_remembered,
        next_remembered: AtomicUsize::new(0),
        sites: sites.found_counters(),
        pool: WorkPool::new(),
        finished: Mutex::new(Vec::new()),
    };
    let mut main = Worker::new(&shared);

    // 1) Root slots, explicit and external (handle stack, global handles).
    for &slot in root_slots {
        main.stats.root_slots_scanned += 1;
        // SAFETY: caller guarantees slot is a valid pointer.
        unsafe { main.visit(slot, None) };
    }
    let main_ptr = &mut main as *mut Worker<'_>;
    external_visit(&mut move |slot: *mut RawGc| {
        // SAFETY: `main` is alive on the surrounding stack frame and no
        // other worker runs yet.
        unsafe {
            (*main_ptr).stats.root_slots_scanned += 1;
            (*main_ptr).visit(slot, None);
        }
    });

    // 2) Remembered parents and every copied body, on every worker.
    if workers > 1 {
        let phase = Phase {
            shared: &shared,
            main: &mut main,
        };
        // SAFETY: `trace_phase` keeps distinct workers apart: index 0 is the
        // collecting thread's `main`, every helper builds its own worker, and
        // shared state is atomic or locked. `phase` outlives the call.
        unsafe {
            threads.run(trace_phase, (&raw const phase).cast());
        }
    } else {
        // SAFETY: STW; the collecting thread is the only worker.
        unsafe { main.drain() };
    }

    // 3) Minor-GC ephemeron processing. This may evacuate values for keys
    // that were already kept alive by ordinary reachability. Then rewrite
    // non-root weak registry entries after all strong and ephemeron
    // reachability has been evacuated, but before from-space is recycled by
    // the flip below.
    // SAFETY: registry slots are valid non-root slots supplied by the heap;
    // the helpers are idle.
    unsafe {
        main.process_ephemeron_fixpoint(ephemeron_registry_slots);
        for &slot in weak_registry_slots {
            main.process_weak_registry_slot(slot);
        }
    }
    process_weak(&mut |mut raw| {
        // SAFETY: owner observations name heap-issued handles. The mutator
        // and helpers are paused, and from-space still holds forwarding words.
        unsafe { main.process_weak_registry_slot(&mut raw) };
        (!raw.is_null()).then_some(raw)
    });

    let main_output = main.finish();
    let helper_outputs = shared
        .finished
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    let mut stats = ScavengeStats {
        workers: 1 + helper_outputs.len(),
        ..ScavengeStats::default()
    };
    for output in std::iter::once(main_output).chain(helper_outputs) {
        remembered_parents.extend(output.remembered);
        if let Some(promoted_gray) = promoted_gray.as_deref_mut() {
            promoted_gray.extend(output.promoted_gray);
        }
        stats.absorb(&output.stats);
    }

    // 4) Finalize and drop dead from-space bodies before recycling their
    // pages. Forwarded bodies were byte-moved to to/old space, so their source
    // payload is logically uninitialized and must never be dropped.
    // SAFETY: every unforwarded header still precedes its original live body.
    unsafe { finalize_and_drop_dead_from_space(new_space, trace_table, host_refs) };

    // 5) Mark the survivors on to-space pages. After the flip below these
    // pages become the new from-space; the mutator allocates above the mark
    // and the next scavenge promotes only what lies below it.
    for page in new_space.to_pages() {
        let h = page.header_mut();
        h.age_mark = h.bump_cursor;
    }

    // 6) Flip from↔to. The preflight's allocated-byte total is the exact
    // physical input, not the larger page-capacity reservation bound. Every
    // survivor contributes its aligned header size to exactly one counter.
    new_space.flip();
    let retained_nursery_bytes = stats.copied_bytes.saturating_add(stats.promoted_bytes);
    debug_assert!(retained_nursery_bytes <= allocated_nursery_bytes);
    stats.reclaimed_bytes = allocated_nursery_bytes.saturating_sub(retained_nursery_bytes);

    Ok(stats)
}

/// Context of the parallel tracing phase.
struct Phase<'s> {
    shared: &'s Shared,
    /// The collecting thread's worker, driven only by index 0.
    main: *mut Worker<'s>,
}

/// One worker's part of the parallel tracing phase.
///
/// # Safety
///
/// `context` is a live [`Phase`]; index 0 runs only on the collecting thread.
unsafe fn trace_phase(context: *const (), index: usize) {
    // SAFETY: per docstring; `GcThreads::run` keeps the phase alive.
    unsafe {
        let phase = &*context.cast::<Phase<'_>>();
        if index == 0 {
            (*phase.main).drain();
        } else {
            phase.shared.pool.join();
            let mut worker = Worker::new(phase.shared);
            worker.drain();
            let output = worker.finish();
            phase
                .shared
                .finished
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(output);
        }
    }
}

/// Run reclamation hooks for every tracked young body that was not
/// evacuated, and keep tracking the ones that stayed young.
///
/// Only bodies whose type has a hook are tracked, at allocation; the dead
/// cells around them are never visited. A copying collector transfers
/// ownership of a survivor's payload with the byte copy in evacuation; only
/// an unforwarded source remains a valid Rust value. Its finalizer follows
/// the same ordering as old-space sweep, then its `Drop` implementation
/// releases owned buffers and host resources before the semispace pages are
/// reset. A promoted survivor leaves the list: old-space sweep owns it.
///
/// # Safety
///
/// Must run under the scavenge STW pause after reachability has converged and
/// before [`NewSpace::flip`] resets from-space.
unsafe fn finalize_and_drop_dead_from_space(
    new_space: &mut NewSpace,
    trace_table: &TraceTable,
    host_refs: &mut crate::host_refs::HostRefTable,
) {
    let cage = cage_base();
    // SAFETY: per docstring; the trace table matches every allocated tag.
    unsafe {
        #[cfg(debug_assertions)]
        verify_cleanup_tracked(new_space, trace_table);
        let tracked = new_space.take_cleanup();
        for offset in tracked {
            let header = cage.add(offset as usize).cast::<GcHeader>();
            if (*header).is_forwarded() {
                let target = GcHeader::read_forwarding_offset(header);
                if (*cage.add(target as usize).cast::<GcHeader>()).is_young() {
                    new_space.track_cleanup(target);
                }
                continue;
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
        }
    }
}

/// Debug check that every from-space body needing reclamation is tracked.
///
/// # Safety
///
/// As [`finalize_and_drop_dead_from_space`], before it takes the list.
#[cfg(debug_assertions)]
unsafe fn verify_cleanup_tracked(new_space: &NewSpace, trace_table: &TraceTable) {
    let cage = cage_base();
    // SAFETY: forwarded from the caller.
    unsafe {
        for page in new_space.from_pages() {
            page.for_each_object(|header, _| {
                let tag = (*header).type_tag();
                if !(*header).is_forwarded() && trace_table.needs_cleanup(tag) {
                    let offset = (header as usize - cage as usize) as u32;
                    debug_assert!(
                        new_space.tracks_cleanup(offset),
                        "young {:?} at {offset:#x} has a reclamation hook but is untracked",
                        trace_table.name(tag),
                    );
                }
            });
        }
    }
}

/// `OTTER_GC_VERIFY=1` turns on per-slot scavenge validation: every child a
/// slot points at is sanity-checked (plausible header size / non-zero type tag)
/// and every corrupt one is reported with the slot's provenance — in-cage
/// (an object field) vs out-of-cage (a root: register stack, anchor stack,
/// handle scope) — plus its parent. A diagnostic for use-after-move / dangling
/// root regressions; off by default (one relaxed atomic load per slot when on).
#[inline]
pub(crate) fn gc_verify_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0); // 0 = unknown, 1 = off, 2 = on
    match STATE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = std::env::var_os("OTTER_GC_VERIFY").is_some_and(|v| v != "0");
            STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// Report a slot whose target is not a plausible live object. See
/// [`gc_verify_enabled`]. Never mutates heap state — pure diagnostic.
#[cold]
unsafe fn verify_child_slot(
    table: &TraceTable,
    slot: *mut RawGc,
    raw: u32,
    header_ptr: *const GcHeader,
    parent_header: Option<*mut GcHeader>,
) {
    // SAFETY: `header_ptr` is an in-cage address by construction; reading the
    // header word is safe even if the object is stale (still mapped memory).
    let (size, tag) = unsafe { ((*header_ptr).size_bytes(), (*header_ptr).type_tag()) };
    if table.header_could_be_live(raw, size, tag) {
        return; // plausible object
    }
    let slot_off = (slot as usize).wrapping_sub(cage_base() as usize);
    let region = if slot_off < (1usize << 32) {
        "object-field (in-cage)"
    } else {
        "root (out-of-cage: register/anchor/handle stack)"
    };
    let (parent_offset, parent_size, parent_tag) = parent_header.map_or((0, 0, 0), |parent| {
        // SAFETY: a parent supplied by the tracer points at the live object
        // whose side-storage slot is being visited.
        unsafe {
            (
                (parent as usize).wrapping_sub(cage_base() as usize) as u32,
                (*parent).size_bytes(),
                (*parent).type_tag(),
            )
        }
    });
    let parent_dump = parent_header.map_or(String::new(), |parent| {
        // SAFETY: the parent header is live and `parent_size` bounds its cell.
        let bytes = unsafe {
            std::slice::from_raw_parts(parent.cast::<u8>(), (*parent).size_bytes() as usize)
        };
        let at = (slot as usize).wrapping_sub(parent as usize);
        let hex: String = bytes.iter().take(64).map(|b| format!("{b:02x}")).collect();
        format!(" slot_in_parent={at} parent_bytes={hex}")
    });
    eprintln!(
        "OTTER_GC_VERIFY: corrupt slot -> raw_offset={raw:#x} target_size={size} target_tag={tag} \
         slot={slot:p} region={region} parent_offset={parent_offset:#x} \
         parent_size={parent_size} parent_tag={parent_tag}{parent_dump}"
    );
    if std::env::var_os("OTTER_GC_VERIFY_BT").is_some() {
        eprintln!("{}", std::backtrace::Backtrace::force_capture());
    }
}

impl<'s> Worker<'s> {
    fn new(shared: &'s Shared) -> Self {
        Self {
            shared,
            local: Vec::new(),
            to_page: std::ptr::null_mut(),
            lab_cursor: 0,
            lab_end: 0,
            remembered: Vec::new(),
            promoted_gray: Vec::new(),
            stats: ScavengeStats::default(),
            in_dirty_scan: false,
            parent_old: false,
        }
    }

    /// Give the unused linear area back and hand over what this worker found.
    fn finish(mut self) -> WorkerOutput {
        let unused = (self.lab_end - self.lab_cursor) as usize;
        if unused != 0 {
            let mut old_space = self.shared.old_space();
            // SAFETY: the lock grants exclusive access to the old space; the
            // range is this worker's untouched area tail.
            unsafe { (*old_space.0).return_lab(self.lab_cursor, unused) };
            self.lab_cursor = self.lab_end;
        }
        WorkerOutput {
            remembered: std::mem::take(&mut self.remembered),
            promoted_gray: std::mem::take(&mut self.promoted_gray),
            stats: self.stats,
        }
    }

    /// Visit one slot: the hot part, inlined into every trace visitor. A
    /// null, old or large target returns at once; only a from-space target
    /// leaves through the out-of-line evacuation.
    ///
    /// # Safety
    ///
    /// `slot` must be a dereferenceable `*mut RawGc` under the STW pause.
    #[inline(always)]
    unsafe fn visit(&mut self, slot: *mut RawGc, parent_header: Option<*mut GcHeader>) {
        // SAFETY: slot is dereferenceable per precondition.
        unsafe {
            if self.in_dirty_scan {
                // Count both remembered-parent scans and the promoted-body
                // closure whose children use the same immediate-promotion
                // policy.
                self.stats.slots_scanned += 1;
            }
            let raw = load_slot(slot);
            if raw == 0 {
                return;
            }
            let header_ptr = self.shared.cage.add(raw as usize) as *mut GcHeader;
            if self.shared.verify {
                verify_child_slot(
                    &*self.shared.trace_table,
                    slot,
                    raw,
                    header_ptr,
                    parent_header,
                );
            }
            // Classify the target by its page: an old or large target is
            // never touched, so its header stays out of the cache. Objects on
            // new-space pages are exactly the young ones.
            match Page::space_of(header_ptr as *const u8) {
                SpaceKind::Old | SpaceKind::Large => {}
                // A young large cell stays where it is; the first visit
                // promotes it, so the slot never needs remembering.
                SpaceKind::NewLarge => {
                    if (*header_ptr).is_young() {
                        self.promote_large(header_ptr);
                    }
                }
                SpaceKind::NewTo => {
                    // Already evacuated during this scavenge: still young.
                    if self.parent_old {
                        self.remember(parent_header);
                    }
                }
                SpaceKind::NewFrom => self.evacuate_slot(slot, header_ptr, parent_header),
            }
        }
    }

    /// Evacuate a from-space target and rewrite its slot.
    ///
    /// # Safety
    ///
    /// `slot` holds `header`, a live from-space object, under the STW pause.
    #[inline(never)]
    unsafe fn evacuate_slot(
        &mut self,
        slot: *mut RawGc,
        header: *mut GcHeader,
        parent_header: Option<*mut GcHeader>,
    ) {
        // SAFETY: per docstring.
        unsafe {
            let new_offset = self.evacuate(header);
            store_slot(slot, new_offset);
            self.stats.slot_updates += 1;
            // Generational invariant: evacuation itself can mint an
            // old->young edge — the parent may already be old/promoted
            // while the child was copied to to-space. Without remembering
            // the parent the next scavenge never rescans that edge and the
            // child dangles after it moves again. Record the parent object,
            // not the slot: traced slots can live in malloc-owned side
            // storage (Box/Vec/SmallVec) outside the cage, while re-tracing
            // the whole parent reaches every slot through the refreshed
            // slab base.
            if self.parent_old
                && (*self.shared.cage.add(new_offset as usize).cast::<GcHeader>()).is_young()
            {
                self.remember(parent_header);
            }
        }
    }

    /// Record an old/large parent that still points at a young child after
    /// slot processing, so the next scavenge re-traces it. Deduped by the
    /// header's remembered bit: a parent is pushed at most once per
    /// scavenge. Roots have no parent and record nothing. Only the worker
    /// tracing a parent records it.
    ///
    /// # Safety
    ///
    /// `parent_header`, when present, must be a live old or large object
    /// header under the current STW scavenge.
    #[cold]
    unsafe fn remember(&mut self, parent_header: Option<*mut GcHeader>) {
        // SAFETY: preconditions are inherited from `visit`.
        unsafe {
            let Some(parent_header) = parent_header else {
                return;
            };
            debug_assert!(is_old_body(parent_header));
            if !(*parent_header).is_remembered() {
                (*parent_header).set_remembered();
                // Parent is old/large and in-cage; old objects do not move
                // on a minor GC, so this offset stays valid until the next
                // scavenge drains it.
                let offset = (parent_header as usize - self.shared.cage as usize) as u32;
                self.remembered.push(RawGc(offset));
            }
        }
    }

    /// Promote a young large cell in place: the visit that clears its young
    /// bit queues it for tracing as a promoted body; the heap moves its page
    /// to the old large object space after the scavenge.
    ///
    /// # Safety
    ///
    /// `header` must reference a cell on a young large object page.
    #[inline(never)]
    unsafe fn promote_large(&mut self, header: *mut GcHeader) {
        // SAFETY: per docstring; only the winning visit writes the header.
        unsafe {
            if !(*header).try_promote_in_place() {
                return;
            }
            let offset = (header as usize - self.shared.cage as usize) as u32;
            let site = (*header).allocation_site();
            if site != 0 {
                self.shared.sites.record_found(site);
                GcHeader::set_allocation_site(header, 0);
            }
            if self.shared.marking {
                // An old cell born while marking runs: the marker visits it.
                (*header).set_mark_color(crate::header::MarkColor::Gray);
                self.promoted_gray.push(RawGc(offset));
            }
            self.stats.promoted_large_bytes += (*header).size_bytes() as usize;
            self.local.push(offset | PROMOTED_ENTRY);
        }
    }

    /// Evacuate one young-gen object: return its copy if another visit
    /// already moved it, otherwise claim, copy, publish the forwarding
    /// offset and queue the copy for tracing.
    ///
    /// # Safety
    ///
    /// `header` must reference a from-space object.
    unsafe fn evacuate(&mut self, header: *mut GcHeader) -> u32 {
        // SAFETY: header is a from-space object per caller. Every
        // dereference goes through raw pointers so no shared reference
        // outlives a write through the same allocation.
        unsafe {
            let claimed = match GcHeader::claim_forwarding(header) {
                ForwardingClaim::Forwarded(offset) => return offset,
                ForwardingClaim::Claimed(flags) => flags,
            };
            let size = (*header).size_bytes() as usize;
            let aligned = align_up(size, CELL_SIZE);
            // A young object reached from a remembered old parent promotes
            // immediately: leaving it young re-remembers the parent, and the
            // parent is then re-traced in full on EVERY scavenge until the
            // child ages out — for a large promoted graph that made each
            // scavenge O(old heap) and the run quadratic (the earley-boyer
            // benchmark spent 20+ minutes inside the scavenger). An object an
            // old object points at is long-lived with overwhelming likelihood
            // anyway, so this is the standard generational answer.
            let promote = self.in_dirty_scan || {
                let page_base = Page::page_base_of(header as *const u8) as usize;
                (header as usize) - page_base < Page::age_mark_of(header as *const u8)
            };
            let (new_offset, promoted) = if promote {
                (self.alloc_old(aligned), true)
            } else {
                match self.alloc_to(aligned) {
                    Some(offset) => (offset, false),
                    None => (self.alloc_old(aligned), true),
                }
            };

            // Copy header + payload to the new location, then give the copy
            // the flags its source had before the claim.
            let dest_ptr = self.shared.cage.add(new_offset as usize);
            copy_cell(header as *const u8, dest_ptr, aligned);
            let dest_header = dest_ptr as *mut GcHeader;
            GcHeader::restore_flags(dest_header, claimed);

            // A site-tagged cell is found once: its copy drops the tag.
            let site = (*header).allocation_site();
            if site != 0 {
                self.shared.sites.record_found(site);
                GcHeader::set_allocation_site(dest_header, 0);
            }
            if promoted {
                (*dest_header).promote_to_old();
                if self.shared.marking {
                    // An old cell born while marking runs: the marker visits it.
                    (*dest_header).set_mark_color(crate::header::MarkColor::Gray);
                    self.promoted_gray.push(RawGc(new_offset));
                }
                self.stats.promoted_bytes += aligned;
                self.local.push(new_offset | PROMOTED_ENTRY);
            } else {
                self.stats.copied_bytes += aligned;
                self.local.push(new_offset);
            }
            GcHeader::publish_forwarding(header, new_offset, claimed);
            new_offset
        }
    }

    /// Bump-allocate `size` bytes in a to-space page this worker owns,
    /// claiming the next unclaimed page when the current one is full.
    /// `None` once to-space has no page left.
    ///
    /// # Safety
    ///
    /// STW scavenge; the claimed pages belong to this worker alone.
    unsafe fn alloc_to(&mut self, size: usize) -> Option<u32> {
        loop {
            if !self.to_page.is_null() {
                // SAFETY: this worker claimed the page below.
                if let Some(offset) = unsafe { Page::bump_alloc_owned(self.to_page, size) } {
                    return Some(offset);
                }
            }
            let index = self.shared.next_to_page.fetch_add(1, Ordering::Relaxed);
            if index >= self.shared.to_page_count {
                self.to_page = std::ptr::null_mut();
                return None;
            }
            // SAFETY: `index` is in bounds of the to-space page list, which
            // does not change during the scavenge.
            self.to_page = unsafe { (*self.shared.to_pages.add(index)).base_ptr() };
        }
    }

    /// Allocate `size` bytes for a promoted cell: small cells bump through
    /// this worker's linear area, larger ones and refills take the lock.
    ///
    /// # Safety
    ///
    /// STW scavenge.
    unsafe fn alloc_old(&mut self, size: usize) -> u32 {
        if size <= LAB_OBJECT_MAX {
            if (self.lab_end - self.lab_cursor) as usize >= size {
                let offset = self.lab_cursor;
                self.lab_cursor += size as u32;
                return offset;
            }
            let mut old_space = self.shared.old_space();
            // SAFETY: the lock grants exclusive access to the old space.
            unsafe {
                let unused = (self.lab_end - self.lab_cursor) as usize;
                (*old_space.0).return_lab(self.lab_cursor, unused);
                let (offset, len) = (*old_space.0)
                    .take_lab(size, LAB_BYTES, crate::page::PAGE_PAYLOAD_SIZE)
                    .expect("old-space linear area during scavenge");
                self.lab_cursor = offset + size as u32;
                self.lab_end = offset + len as u32;
                return offset;
            }
        }
        let mut old_space = self.shared.old_space();
        // SAFETY: the lock grants exclusive access to the old space.
        unsafe {
            (*old_space.0)
                .alloc(size)
                .expect("old-space alloc during scavenge")
        }
    }

    /// Trace every queued copy, every remembered parent and every chunk other
    /// workers share until all workers run dry.
    ///
    /// # Safety
    ///
    /// STW scavenge.
    unsafe fn drain(&mut self) {
        loop {
            while let Some(entry) = self.local.pop() {
                // SAFETY: entries name live copies this scavenge made.
                unsafe { self.trace_entry(entry) };
                if self.local.len() > SHARE_THRESHOLD && self.shared.pool.has_idle() {
                    self.share();
                }
            }
            // SAFETY: STW scavenge.
            if unsafe { self.trace_next_remembered() } {
                continue;
            }
            let shared = self.shared;
            match shared.pool.take(|| shared.remembered_exhausted()) {
                Some(chunk) => self.local = chunk,
                None => return,
            }
        }
    }

    /// Trace only this worker's own queue, for the single-threaded phases.
    ///
    /// # Safety
    ///
    /// STW scavenge; no other worker runs.
    unsafe fn drain_local(&mut self) {
        while let Some(entry) = self.local.pop() {
            // SAFETY: entries name live copies this scavenge made.
            unsafe { self.trace_entry(entry) };
        }
    }

    /// Hand the older half of this worker's queue to idle workers.
    fn share(&mut self) {
        let half = self.local.len() / 2;
        let chunk: Vec<Entry> = self.local.drain(..half).collect();
        self.shared.pool.share(chunk);
    }

    /// Re-trace one claimed chunk of remembered parents — the old/large
    /// objects the write barrier (and the prior scavenge's re-dirty path)
    /// recorded as holding an old→young edge. Each is a root for the young
    /// collection: re-tracing it reaches every young edge through the
    /// current body. Type-owned dirty ranges may skip clean old edges
    /// without retaining raw slot addresses. A parent's remembered bit is
    /// cleared just before it is traced, so the re-dirty path re-records
    /// (and re-sets the bit on) a parent that still points young; only the
    /// worker tracing a parent reads or sets its bit. `false` once every
    /// chunk is claimed.
    ///
    /// # Safety
    ///
    /// STW scavenge; the snapshot holds in-cage old/large parent offsets.
    unsafe fn trace_next_remembered(&mut self) -> bool {
        let total = self.shared.remembered.len();
        let start = self
            .shared
            .next_remembered
            .fetch_add(REMEMBERED_CHUNK, Ordering::Relaxed);
        if start >= total {
            return false;
        }
        let end = (start + REMEMBERED_CHUNK).min(total);
        self.in_dirty_scan = true;
        for index in start..end {
            let parent = self.shared.remembered[index];
            self.stats.dirty_cards_scanned += 1; // remembered-set entries scanned
            // SAFETY: per docstring.
            unsafe {
                let header = self.shared.cage.add(parent.0 as usize) as *mut GcHeader;
                (*header).clear_remembered();
                // Skip swept corpses: a full-GC sweep drops dead old/large
                // objects in place (freeing their payload buffers, e.g. a
                // string `Vec<u16>`) but leaves the header walkable. Tracing
                // such a corpse would read its freed — and possibly reused —
                // backing as live GC slots (use-after-free). A full GC also
                // prunes dead parents from the buffer, so this is
                // belt-and-suspenders.
                if (*header).size_bytes() != 0 && !(*header).is_swept() {
                    self.stats.objects_retraced += 1;
                    self.trace_one(header, TraceKind::Remembered);
                }
            }
        }
        self.in_dirty_scan = false;
        true
    }

    /// Trace one queued copy. Children of a freshly promoted body promote
    /// too (same policy as the remembered re-trace): leaving them young
    /// would re-remember every promoted parent and re-trace it on all later
    /// scavenges — the transitive closure of an old graph must reach old
    /// space within this scavenge.
    ///
    /// # Safety
    ///
    /// `entry` names a live copy this scavenge made.
    unsafe fn trace_entry(&mut self, entry: Entry) {
        let promoted = entry & PROMOTED_ENTRY != 0;
        let offset = entry & !PROMOTED_ENTRY;
        self.in_dirty_scan = promoted;
        // SAFETY: per docstring.
        unsafe {
            let header = self.shared.cage.add(offset as usize) as *mut GcHeader;
            self.trace_one(header, TraceKind::Full);
        }
        self.in_dirty_scan = false;
    }

    /// Trace one header — the visitor evacuates any from-space child. The
    /// tracing scope is independent of the policy that promotes an old
    /// body's children: newly promoted bodies require full tracing even
    /// under that policy.
    ///
    /// # Safety
    ///
    /// `header` is a live, valid GcHeader this worker alone traces.
    unsafe fn trace_one(&mut self, header: *mut GcHeader, kind: TraceKind) {
        // SAFETY: per docstring; the trace table registers the type tag.
        unsafe {
            self.parent_old = is_old_body(header);
            let table = self.shared.trace_table;
            let this = self as *mut Self;
            let mut visitor = move |slot: *mut RawGc| {
                (*this).visit(slot, Some(header));
            };
            match kind {
                TraceKind::Full => (*table).trace(header, &mut visitor),
                TraceKind::Remembered => (*table).trace_remembered(header, &mut visitor),
            }
        }
    }

    /// Update a non-root registry slot if its young target survived.
    ///
    /// # Safety
    ///
    /// `slot` must be a dereferenceable registry slot; no other worker
    /// runs. Unlike [`Self::visit`], this never evacuates the target; it
    /// observes whether ordinary strong tracing already forwarded the object.
    unsafe fn process_weak_registry_slot(&mut self, slot: *mut RawGc) {
        // SAFETY: slot is dereferenceable per precondition.
        unsafe {
            let raw = (*slot).0;
            if raw == 0 {
                return;
            }
            let header_ptr = self.shared.cage.add(raw as usize) as *mut GcHeader;
            if !(*header_ptr).is_young() {
                return;
            }
            if Page::space_of(header_ptr as *const u8) == SpaceKind::NewTo {
                return;
            }
            if (*header_ptr).is_forwarded() {
                (*slot).0 = GcHeader::read_forwarding_offset(header_ptr);
                self.stats.slot_updates += 1;
            } else {
                *slot = RawGc::NULL;
            }
        }
    }

    /// Update a non-root registry slot if it is currently known live.
    ///
    /// Returns `true` when the slot points at an old object or a forwarded
    /// young object. Young objects that have not been forwarded are left
    /// untouched here because an ephemeron value discovered later in the same
    /// fixpoint may still make the table reachable.
    ///
    /// # Safety
    ///
    /// As [`Self::process_weak_registry_slot`].
    unsafe fn update_registry_slot_if_forwarded(&mut self, slot: *mut RawGc) -> bool {
        // SAFETY: slot is dereferenceable per precondition.
        unsafe {
            let raw = (*slot).0;
            if raw == 0 {
                return false;
            }
            let header_ptr = self.shared.cage.add(raw as usize) as *mut GcHeader;
            if !(*header_ptr).is_young() {
                return true;
            }
            if Page::space_of(header_ptr as *const u8) == SpaceKind::NewTo {
                return true;
            }
            if (*header_ptr).is_forwarded() {
                (*slot).0 = GcHeader::read_forwarding_offset(header_ptr);
                self.stats.slot_updates += 1;
                return true;
            }
            false
        }
    }

    /// Update a weak ephemeron key slot.
    ///
    /// Returns `true` when the key is live for this minor collection. Dead
    /// young keys are nulled in place so VM weak-table lookups stop
    /// observing them.
    ///
    /// # Safety
    ///
    /// As [`Self::process_weak_registry_slot`].
    unsafe fn process_ephemeron_key_slot(&mut self, slot: *mut RawGc) -> bool {
        // SAFETY: slot is dereferenceable per precondition.
        unsafe {
            let raw = (*slot).0;
            if raw == 0 {
                return false;
            }
            let header_ptr = self.shared.cage.add(raw as usize) as *mut GcHeader;
            if !(*header_ptr).is_young() {
                return true;
            }
            if Page::space_of(header_ptr as *const u8) == SpaceKind::NewTo {
                return true;
            }
            if (*header_ptr).is_forwarded() {
                (*slot).0 = GcHeader::read_forwarding_offset(header_ptr);
                self.stats.slot_updates += 1;
                return true;
            }
            *slot = RawGc::NULL;
            self.stats.slot_updates += 1;
            false
        }
    }

    /// Process ephemeron tables until no more young keys/tables/values are
    /// forwarded. This is a young-generation analogue of the old-generation
    /// ephemeron fixpoint: keys are weak, values become strong only for keys
    /// already proven live.
    ///
    /// # Safety
    ///
    /// `ephemeron_registry_slots` must address valid non-root registry
    /// slots; no other worker runs.
    unsafe fn process_ephemeron_fixpoint(&mut self, ephemeron_registry_slots: &[*mut RawGc]) {
        // SAFETY: caller guarantees slot validity under STW.
        unsafe {
            loop {
                // Fixpoint progress = new evacuations only: a re-scanned slot
                // that merely rewrites to an existing forwarding target is
                // not progress.
                let before = (
                    self.stats.copied_bytes,
                    self.stats.promoted_bytes,
                    self.stats.promoted_large_bytes,
                );

                for &slot in ephemeron_registry_slots {
                    if !self.update_registry_slot_if_forwarded(slot) {
                        continue;
                    }
                    let raw = (*slot).0;
                    if raw == 0 {
                        continue;
                    }
                    let header_ptr = self.shared.cage.add(raw as usize) as *mut GcHeader;
                    self.trace_ephemeron_table(header_ptr);
                }

                self.drain_local();

                if (
                    self.stats.copied_bytes,
                    self.stats.promoted_bytes,
                    self.stats.promoted_large_bytes,
                ) == before
                {
                    break;
                }
            }

            for &slot in ephemeron_registry_slots {
                self.process_weak_registry_slot(slot);
            }
        }
    }

    /// Trace one live ephemeron table.
    ///
    /// # Safety
    ///
    /// `header` is a live table header whose type tag is registered; no
    /// other worker runs.
    unsafe fn trace_ephemeron_table(&mut self, header: *mut GcHeader) {
        // SAFETY: per docstring.
        unsafe {
            let table = self.shared.trace_table;
            self.parent_old = is_old_body(header);
            let this = self as *mut Self;
            let mut visitor = move |key_slot: *mut RawGc,
                                    visit_value_slots: &mut crate::trace::EphemeronValueVisitor<
                '_,
            >| {
                if (*this).process_ephemeron_key_slot(key_slot) {
                    let mut strong_visitor = |slot: *mut RawGc| {
                        (*this).visit(slot, Some(header));
                    };
                    visit_value_slots(&mut strong_visitor);
                }
            };
            (*table).trace_ephemerons(header, &mut visitor);
        }
    }
}

/// Copy one whole cell, `bytes` a multiple of the cell size.
///
/// Most surviving cells are a few words long, and a call into the general
/// copy routine costs more than moving them: each small size is a fixed-size
/// copy the compiler emits as plain loads and stores. Copying the aligned
/// extent reads only the cell's own padding past its size.
///
/// # Safety
///
/// `src` and `dst` are cell-aligned, non-overlapping and valid for `bytes`.
#[inline(always)]
unsafe fn copy_cell(src: *const u8, dst: *mut u8, bytes: usize) {
    const WORD: usize = 8;
    const _: () = assert!(CELL_SIZE.is_multiple_of(WORD));
    #[inline(always)]
    unsafe fn words<const N: usize>(src: *const u8, dst: *mut u8) {
        // SAFETY: forwarded from `copy_cell`.
        unsafe {
            core::ptr::copy_nonoverlapping(src.cast::<[u64; N]>(), dst.cast::<[u64; N]>(), 1);
        }
    }
    // SAFETY: forwarded from the caller; every arm copies exactly `bytes`.
    unsafe {
        match bytes / WORD {
            1 => words::<1>(src, dst),
            2 => words::<2>(src, dst),
            3 => words::<3>(src, dst),
            4 => words::<4>(src, dst),
            5 => words::<5>(src, dst),
            6 => words::<6>(src, dst),
            7 => words::<7>(src, dst),
            8 => words::<8>(src, dst),
            9 => words::<9>(src, dst),
            10 => words::<10>(src, dst),
            11 => words::<11>(src, dst),
            12 => words::<12>(src, dst),
            13 => words::<13>(src, dst),
            14 => words::<14>(src, dst),
            15 => words::<15>(src, dst),
            16 => words::<16>(src, dst),
            _ => core::ptr::copy_nonoverlapping(src, dst, bytes),
        }
    }
}

/// Whether `header` lives on an old or large page.
///
/// # Safety
///
/// `header` is a live object header inside a heap page.
#[inline]
unsafe fn is_old_body(header: *const GcHeader) -> bool {
    // SAFETY: per docstring, the page header precedes the object.
    match unsafe { Page::space_of(header.cast::<u8>()) } {
        SpaceKind::Old | SpaceKind::Large => true,
        // Promoted in place earlier in this scavenge.
        SpaceKind::NewLarge => unsafe { !(*header).is_young() },
        _ => false,
    }
}

/// Whether a body came from the old remembered set or a worklist.
enum TraceKind {
    Full,
    Remembered,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressed::{Cage, RawGc};
    use crate::header::{GcHeader, HEADER_SIZE};
    use crate::page::{CELL_SIZE, PAGE_HEADER_SIZE, PAGE_SIZE, Page, SpaceKind, align_up};
    use crate::space::{NewSpace, OldSpace};
    use crate::trace::{SlotVisitor, TraceTable, Traceable};

    #[derive(Debug)]
    struct SelfRelativeSlot {
        cached_slot: *mut RawGc,
        child: RawGc,
    }

    impl Traceable for SelfRelativeSlot {
        const TYPE_TAG: u8 = 0xD1;

        unsafe fn trace_slots(this: *mut Self, visitor: &mut SlotVisitor<'_>) {
            unsafe {
                let child_slot = std::ptr::addr_of_mut!((*this).child);
                (*this).cached_slot = child_slot;
                if !(*child_slot).is_null() {
                    visitor(child_slot);
                }
            }
        }

        unsafe fn trace_remembered_slots(_this: *mut Self, _visitor: &mut SlotVisitor<'_>) {
            panic!("newly copied or promoted bodies require the full trace callback");
        }
    }

    #[test]
    fn promoted_body_uses_full_trace_to_keep_its_only_child() {
        use crate::{EmptyRoots, GcHeap, HandleScope, test_support::OpaqueLeaf};

        let mut heap = GcHeap::new().expect("heap");
        // SAFETY: heap outlives the scope and its handles.
        let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
        let child = heap.alloc(OpaqueLeaf { payload: 79 }).expect("child");
        let parent = heap
            .alloc(SelfRelativeSlot {
                cached_slot: std::ptr::null_mut(),
                child: child.raw(),
            })
            .expect("parent");
        let parent = scope.local(parent);
        heap.collect_minor(EmptyRoots).expect("copy");
        heap.collect_minor(EmptyRoots).expect("promote");
        // SAFETY: the live parent root still owns its registered leaf child.
        let child =
            unsafe { heap.read_payload(parent.get(), |body| body.child.cast::<OpaqueLeaf>()) };
        assert_eq!(heap.read_payload(child, |body| body.payload), 79);
        // SAFETY: the rooted parent and its traced child are live after promotion.
        assert!(unsafe { (*parent.get().as_header_ptr()).is_old() });
        // SAFETY: as above, the leaf remains owned by the rooted parent.
        assert!(unsafe { (*child.as_header_ptr()).is_old() });
    }

    #[test]
    fn old_space_fallback_promotes_and_traces_moved_body() {
        Cage::ensure_default().expect("cage");

        let mut new_space = NewSpace::new(1).expect("new space");
        let mut old_space = OldSpace::new();
        let mut trace_table = TraceTable::new();
        trace_table.register::<SelfRelativeSlot>();
        let mut remembered = Vec::new();

        let total = HEADER_SIZE + std::mem::size_of::<SelfRelativeSlot>();
        let aligned = align_up(total, CELL_SIZE);
        let offset = new_space.alloc(aligned).expect("from-space allocation");

        let header = unsafe { cage_base().add(offset as usize) as *mut GcHeader };
        let payload = unsafe {
            cage_base()
                .add(offset as usize + HEADER_SIZE)
                .cast::<SelfRelativeSlot>()
        };
        unsafe {
            std::ptr::write(
                header,
                GcHeader::new_young(SelfRelativeSlot::TYPE_TAG, aligned as u32),
            );
            std::ptr::write(
                payload,
                SelfRelativeSlot {
                    cached_slot: std::ptr::null_mut(),
                    child: RawGc::NULL,
                },
            );
            (*payload).cached_slot = std::ptr::addr_of_mut!((*payload).child);
        }

        let to_payload_capacity = align_up(PAGE_SIZE - PAGE_HEADER_SIZE, CELL_SIZE);
        assert!(
            new_space.to_pages()[0]
                .bump_alloc(to_payload_capacity)
                .is_some(),
            "test must leave to-space without evacuation room"
        );

        let mut root = RawGc(offset);
        let stats = unsafe {
            scavenge(
                &mut new_space,
                &mut old_space,
                &trace_table,
                &[&mut root as *mut RawGc],
                &mut |_| {},
                &[],
                &[],
                &mut |_| {},
                &mut remembered,
                &mut crate::host_refs::HostRefTable::default(),
                &mut crate::pretenuring::AllocationSites::default(),
                &mut crate::gc_threads::GcThreads::default(),
                None,
            )
        }
        .expect("promotion preflight");

        assert_eq!(stats.promoted_bytes, total);
        assert_eq!(stats.copied_bytes, 0);

        let moved_header = unsafe { cage_base().add(root.0 as usize).cast::<GcHeader>() };
        assert_eq!(
            unsafe { Page::header_of(moved_header.cast()).space },
            SpaceKind::Old
        );
        assert!(
            unsafe { (*moved_header).is_old() },
            "old-space fallback destination must not keep a young header"
        );

        let moved_payload = unsafe {
            cage_base()
                .add(root.0 as usize + HEADER_SIZE)
                .cast::<SelfRelativeSlot>()
        };
        let expected_slot = unsafe { std::ptr::addr_of_mut!((*moved_payload).child) };
        assert_eq!(
            unsafe { (*moved_payload).cached_slot },
            expected_slot,
            "relocation trace must refresh self-relative payload pointers"
        );
    }
}
