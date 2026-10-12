//! Old-space sweeping on the heap's helper threads.
//!
//! # Contents
//! - [`sweep_old_pages`] — walk every old page once: whiten survivors, cover
//!   each maximal dead run with a free filler, set the page's allocated bytes
//!   and bump tail, and collect the free ranges, live statistics and the dead
//!   cells whose reclamation hooks must run.
//! - [`OldSweep`] — what one walk found.
//!
//! # Invariants
//! - Each page is walked by exactly one thread, which alone writes its
//!   header and cells. Pages are claimed by an atomic index.
//! - No reclamation hook runs here: a dead cell with a drop, finalize or
//!   host-release hook is recorded with its tag and reclaimed by the
//!   collecting thread afterwards. A filler overwrites only the header at the
//!   start of its run, and free ranges are listed only after the hooks ran,
//!   so every recorded payload is intact when its hook runs.
//! - A page without live bytes is reaped whole: its walk only records hooks.
//!
//! # See also
//! - `GcHeap::sweep_phase` — the caller, which runs the hooks, reaps empty
//!   pages and publishes the free ranges.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::gc_threads::GcThreads;
use crate::header::{FREE_TAG, GcHeader};
use crate::page::{CELL_SIZE, Page, align_up};
use crate::stats::TYPE_TAG_COUNT;
use crate::trace::TraceTable;

/// Pages one thread claims at a time.
const PAGE_CHUNK: usize = 4;

/// What a sweep of old pages found.
pub(super) struct OldSweep {
    /// Live cells by type tag.
    pub(super) live_count: [u64; TYPE_TAG_COUNT],
    /// Live cell bytes by type tag.
    pub(super) live_bytes: [usize; TYPE_TAG_COUNT],
    /// Bytes of cells found dead this sweep.
    pub(super) reclaimed: usize,
    /// Allocated bytes the surviving pages keep.
    pub(super) retained: u64,
    /// Free ranges recovered from partially live pages, page-absolute cage
    /// offset and length.
    pub(super) free_ranges: Vec<(u32, usize)>,
    /// Dead cells with a reclamation hook: header address and type tag.
    pub(super) reclaim: Vec<(usize, u8)>,
}

impl Default for OldSweep {
    fn default() -> Self {
        Self {
            live_count: [0; TYPE_TAG_COUNT],
            live_bytes: [0; TYPE_TAG_COUNT],
            reclaimed: 0,
            retained: 0,
            free_ranges: Vec::new(),
            reclaim: Vec::new(),
        }
    }
}

impl OldSweep {
    fn absorb(&mut self, other: Self) {
        for tag in 0..TYPE_TAG_COUNT {
            self.live_count[tag] = self.live_count[tag].wrapping_add(other.live_count[tag]);
            self.live_bytes[tag] = self.live_bytes[tag].wrapping_add(other.live_bytes[tag]);
        }
        self.reclaimed += other.reclaimed;
        self.retained = self.retained.saturating_add(other.retained);
        self.free_ranges.extend(other.free_ranges);
        self.reclaim.extend(other.reclaim);
    }
}

/// Sweep `pages`, on `threads` as well when `parallel`.
///
/// # Safety
///
/// STW pause after a completed mark: every page is a live old page whose
/// headers up to its bump cursor are valid, and colors are final.
pub(super) unsafe fn sweep_old_pages(
    pages: &[Page],
    trace_table: &TraceTable,
    threads: &mut GcThreads,
    parallel: bool,
) -> OldSweep {
    let job = SweepJob {
        pages: pages.as_ptr(),
        count: pages.len(),
        next: AtomicUsize::new(0),
        trace_table,
        found: Mutex::new(Vec::new()),
    };
    // SAFETY: workers claim disjoint pages; `job` outlives the call.
    unsafe {
        if parallel && threads.workers() > 1 {
            threads.run(sweep_worker, (&raw const job).cast());
        } else {
            sweep_worker((&raw const job).cast(), 0);
        }
    }
    let mut total = OldSweep::default();
    for found in job
        .found
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
    {
        total.absorb(found);
    }
    total
}

struct SweepJob {
    pages: *const Page,
    count: usize,
    next: AtomicUsize,
    trace_table: *const TraceTable,
    found: Mutex<Vec<OldSweep>>,
}

// SAFETY: pages are claimed exclusively through `next`; the trace table is
// only read; results are handed over under the lock.
unsafe impl Sync for SweepJob {}

/// One worker: sweep claimed pages until none is left.
///
/// # Safety
///
/// `context` is a live [`SweepJob`] under the STW pause.
unsafe fn sweep_worker(context: *const (), _index: usize) {
    // SAFETY: per docstring.
    let job = unsafe { &*context.cast::<SweepJob>() };
    // SAFETY: the job's trace table outlives the sweep.
    let trace_table = unsafe { &*job.trace_table };
    let mut found = OldSweep::default();
    loop {
        let start = job.next.fetch_add(PAGE_CHUNK, Ordering::Relaxed);
        if start >= job.count {
            break;
        }
        for index in start..(start + PAGE_CHUNK).min(job.count) {
            // SAFETY: this worker claimed `index`; the page list does not
            // change during the sweep.
            unsafe { sweep_page(&*job.pages.add(index), trace_table, &mut found) };
        }
    }
    job.found
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(found);
}

/// Sweep one old page into `found`.
///
/// Drop dead objects in place, then cover every maximal dead run with a
/// `FREE_TAG` filler so the page's linear header walk stays intact and the
/// range becomes reusable through the free list. A run that ends at the bump
/// cursor is returned to bump space instead. Survivors are whitened for the
/// next cycle and counted.
///
/// # Safety
///
/// As [`sweep_old_pages`]; this thread alone owns `page` during the call.
unsafe fn sweep_page(page: &Page, trace_table: &TraceTable, found: &mut OldSweep) {
    // SAFETY: per docstring.
    unsafe {
        let record = |h: *mut GcHeader, found: &mut OldSweep| {
            let tag = (*h).type_tag();
            if trace_table.needs_cleanup(tag) {
                found.reclaim.push((h as usize, tag));
            }
            (*h).set_swept();
            found.reclaimed += (*h).size_bytes() as usize;
        };
        // A fully-dead page is reaped whole — recording its ranges would
        // leave dangling free-list entries — but its corpses still need their
        // one finalize/drop pass (backing stores, external-memory tokens)
        // before the page memory returns to the cage.
        if page.header().live_bytes == 0 {
            page.for_each_object(|h, _| {
                if (*h).type_tag() != FREE_TAG && !(*h).is_swept() {
                    record(h, found);
                }
            });
            return;
        }
        let page_base_offset = page.cage_offset();
        let base = page.base_ptr();
        let mut live_allocated = 0usize;
        let mut run_start: Option<usize> = None;
        let mut run_bytes = 0usize;
        let close_run =
            |run_start: &mut Option<usize>, run_bytes: &mut usize, found: &mut OldSweep| {
                if let Some(start) = run_start.take() {
                    let bytes = std::mem::take(run_bytes);
                    // The run lies inside this page's payload area; every
                    // byte of it belonged to swept objects.
                    let header_ptr = base.add(start) as *mut GcHeader;
                    std::ptr::write(header_ptr, GcHeader::new_free(bytes as u32));
                    found
                        .free_ranges
                        .push((page_base_offset + start as u32, bytes));
                }
            };
        page.for_each_object(|h, offset| {
            let tag = (*h).type_tag();
            let size = (*h).size_bytes() as usize;
            let aligned = align_up(size, CELL_SIZE);
            let is_free_filler = tag == FREE_TAG;
            if is_free_filler || !(*h).is_marked() {
                // Reclaim exactly once — a second `drop_in_place` would
                // double-free any buffer the payload owns. Fillers own
                // nothing.
                if !is_free_filler && !(*h).is_swept() {
                    record(h, found);
                }
                if run_start.is_none() {
                    run_start = Some(offset);
                }
                run_bytes += aligned;
            } else {
                // Whiten the survivor for the next cycle.
                (*h).clear_mark();
                let tag = tag as usize;
                found.live_count[tag] = found.live_count[tag].wrapping_add(1);
                found.live_bytes[tag] = found.live_bytes[tag].wrapping_add(size);
                live_allocated += aligned;
                close_run(&mut run_start, &mut run_bytes, found);
            }
        });
        let header = page.header_mut();
        if let Some(start) = run_start {
            // The final dead run touches the bump cursor: hand it straight
            // back to bump allocation.
            debug_assert_eq!(start + run_bytes, header.bump_cursor);
            header.bump_cursor = start;
        } else {
            close_run(&mut run_start, &mut run_bytes, found);
        }
        header.allocated_bytes = live_allocated;
        found.retained = found.retained.saturating_add(live_allocated as u64);
    }
}
