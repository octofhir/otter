//! Old-space sweeping: one walk per page, in a collection's pause or on the
//! heap's helper threads while the mutator runs.
//!
//! # Contents
//! - [`OldSweep`] — the old pages of one cycle, claimed one at a time by an
//!   atomic index: by the helpers' background job and by the collecting
//!   thread when it needs room or the cycle's end. Finished pages wait in it
//!   for the collecting thread.
//! - [`SweptPage`] — what one walk found: the page's new bump cursor and
//!   allocated bytes, its free ranges, and the dead cells whose reclamation
//!   hooks must run.
//! - [`SweepTotals`] — live statistics and reclaimed bytes of every walk.
//!
//! # Invariants
//! - Each page is walked by exactly one thread. The walk writes only that
//!   page's dead cells (a filler over each maximal dead run and the swept
//!   flag) and its survivors' mark color through the header's atomic flags.
//!   The page header is written only by the collecting thread, when it
//!   applies the page's [`SweptPage`].
//! - No reclamation hook runs during a walk: a dead cell with a drop,
//!   finalize or host-release hook is recorded with its tag, and the
//!   collecting thread reclaims it before listing any of the page's free
//!   ranges, so every recorded payload is intact when its hook runs.
//! - A page without live bytes is walked only to record hooks; the cycle's
//!   end reaps it whole.
//! - The pages are the old space's pages in the collection's pause. Until a
//!   page's result is applied, nothing allocates into it, reads its header
//!   fields the walk sets, or walks its cells.
//!
//! # See also
//! - `super::sweeping` — the collecting thread's side: starting a cycle,
//!   applying finished pages and completing it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::header::{FREE_TAG, GcHeader};
use crate::page::{CELL_SIZE, PAGE_HEADER_SIZE, Page, PageHeader, align_up};
use crate::stats::TYPE_TAG_COUNT;
use crate::trace::TraceTable;

/// What the walks of one cycle counted.
pub(super) struct SweepTotals {
    /// Live cells by type tag.
    pub(super) live_count: [u64; TYPE_TAG_COUNT],
    /// Live cell bytes by type tag.
    pub(super) live_bytes: [usize; TYPE_TAG_COUNT],
    /// Bytes of cells found dead.
    pub(super) reclaimed: usize,
    /// Allocated bytes the surviving pages keep.
    pub(super) retained: u64,
}

impl Default for SweepTotals {
    fn default() -> Self {
        Self {
            live_count: [0; TYPE_TAG_COUNT],
            live_bytes: [0; TYPE_TAG_COUNT],
            reclaimed: 0,
            retained: 0,
        }
    }
}

impl SweepTotals {
    pub(super) fn absorb(&mut self, other: &Self) {
        for tag in 0..TYPE_TAG_COUNT {
            self.live_count[tag] = self.live_count[tag].wrapping_add(other.live_count[tag]);
            self.live_bytes[tag] = self.live_bytes[tag].wrapping_add(other.live_bytes[tag]);
        }
        self.reclaimed += other.reclaimed;
        self.retained = self.retained.saturating_add(other.retained);
    }
}

/// One walked page, for the collecting thread to apply.
pub(super) struct SweptPage {
    /// The page's base address.
    pub(super) base: usize,
    /// Its bump cursor: a dead run that reached the old cursor returns to
    /// bump space.
    pub(super) bump_cursor: usize,
    /// Bytes its survivors occupy.
    pub(super) allocated: usize,
    /// Whether no cell on it survived: it is reaped whole.
    pub(super) dead: bool,
    /// Filler-capped dead runs, page-absolute cage offset and length.
    pub(super) free_ranges: Vec<(u32, usize)>,
    /// Dead cells with a reclamation hook: header address and type tag.
    pub(super) reclaim: Vec<(usize, u8)>,
}

/// The old pages one cycle sweeps.
pub(super) struct OldSweep {
    pages: Box<[usize]>,
    next: AtomicUsize,
    finished: AtomicUsize,
    trace_table: *const TraceTable,
    swept: Mutex<Vec<SweptPage>>,
    totals: Mutex<SweepTotals>,
}

// SAFETY: pages are claimed exclusively through `next`; the trace table is
// only read and outlives the cycle; results are handed over under locks.
unsafe impl Sync for OldSweep {}

impl OldSweep {
    /// A cycle over `pages`, which the collection's pause just marked.
    pub(super) fn new(pages: &[Page], trace_table: &TraceTable) -> Box<Self> {
        Box::new(Self {
            pages: pages.iter().map(|page| page.base_ptr() as usize).collect(),
            next: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            trace_table,
            swept: Mutex::new(Vec::new()),
            totals: Mutex::new(SweepTotals::default()),
        })
    }

    /// Pages the cycle walks.
    pub(super) fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Whether every page has been claimed by some thread.
    pub(super) fn all_claimed(&self) -> bool {
        self.next.load(Ordering::Relaxed) >= self.pages.len()
    }

    /// Whether every page's result waits or was taken.
    pub(super) fn all_finished(&self) -> bool {
        self.finished() == self.pages.len()
    }

    /// Pages whose walk ended, taken or not.
    pub(super) fn finished(&self) -> usize {
        self.finished.load(Ordering::Acquire)
    }

    /// Claim and walk pages until `limit` were walked or none is left;
    /// returns how many this call walked.
    ///
    /// # Safety
    /// The cycle's pages are live old pages whose headers up to their bump
    /// cursors are valid and whose colors are final, and the invariants of
    /// the module hold for every page not yet applied.
    pub(super) unsafe fn sweep_pages(&self, limit: usize) -> usize {
        // SAFETY: the trace table outlives the cycle.
        let trace_table = unsafe { &*self.trace_table };
        let mut totals = SweepTotals::default();
        let mut walked = 0;
        while walked < limit {
            let index = self.next.fetch_add(1, Ordering::Relaxed);
            if index >= self.pages.len() {
                break;
            }
            // SAFETY: this thread claimed the page; per the caller.
            let page = unsafe { sweep_page(self.pages[index], trace_table, &mut totals) };
            self.swept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(page);
            self.finished.fetch_add(1, Ordering::Release);
            walked += 1;
        }
        if walked != 0 {
            self.totals
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .absorb(&totals);
        }
        walked
    }

    /// The pages walked since the last call.
    pub(super) fn take_swept(&self) -> Vec<SweptPage> {
        std::mem::take(&mut *self.swept.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// The statistics of every walk, once all pages finished.
    pub(super) fn into_totals(self: Box<Self>) -> SweepTotals {
        debug_assert!(self.all_finished());
        self.totals
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The helpers' background job: walk pages until none is left.
///
/// # Safety
/// `context` is a live [`OldSweep`] under [`OldSweep::sweep_pages`]'s
/// conditions until the job is joined.
pub(super) unsafe fn sweep_background(context: *const (), _index: usize) {
    // SAFETY: per docstring.
    unsafe { (*context.cast::<OldSweep>()).sweep_pages(usize::MAX) };
}

/// Walk the old page at `base`.
///
/// Dead cells are recorded once and covered, run by run, with `FREE_TAG`
/// fillers so the page's linear header walk stays intact; a run that ends at
/// the bump cursor goes back to bump space instead. Survivors are whitened
/// for the next cycle and counted.
///
/// # Safety
/// As [`OldSweep::sweep_pages`]; this thread alone walks the page.
unsafe fn sweep_page(base: usize, trace_table: &TraceTable, totals: &mut SweepTotals) -> SweptPage {
    let page = base as *mut u8;
    let header = page.cast::<PageHeader>();
    // SAFETY: per docstring; these header fields stay put until the page's
    // result is applied.
    let (live_bytes, cursor, page_offset) = unsafe {
        (
            (&raw const (*header).live_bytes).read(),
            (&raw const (*header).bump_cursor).read(),
            (&raw const (*header).cage_offset).read(),
        )
    };
    let mut swept = SweptPage {
        base,
        bump_cursor: cursor,
        allocated: 0,
        dead: live_bytes == 0,
        free_ranges: Vec::new(),
        reclaim: Vec::new(),
    };
    let record = |h: *mut GcHeader, swept: &mut SweptPage, totals: &mut SweepTotals| {
        // SAFETY: `h` is a dead cell of this page.
        unsafe {
            let tag = (*h).type_tag();
            if trace_table.needs_cleanup(tag) {
                swept.reclaim.push((h as usize, tag));
            }
            (*h).set_swept();
            totals.reclaimed += (*h).size_bytes() as usize;
        }
    };
    let mut run_start: Option<usize> = None;
    let mut run_bytes = 0usize;
    let mut offset = PAGE_HEADER_SIZE;
    while offset < cursor {
        // SAFETY: every header below the cursor is valid.
        let h = unsafe { page.add(offset) }.cast::<GcHeader>();
        let size = unsafe { (*h).size_bytes() } as usize;
        if size == 0 {
            break;
        }
        let aligned = align_up(size, CELL_SIZE);
        let tag = unsafe { (*h).type_tag() };
        let filler = tag == FREE_TAG;
        // SAFETY: `h` is a cell of this page; its color is final.
        let marked = !filler && unsafe { (*h).is_marked() };
        if swept.dead {
            // Reaped whole: only its corpses' hooks are recorded.
            if !filler && unsafe { !(*h).is_swept() } {
                record(h, &mut swept, totals);
            }
        } else if marked {
            // SAFETY: as above; the color is an atomic flag.
            unsafe { (*h).clear_mark() };
            let index = tag as usize;
            totals.live_count[index] = totals.live_count[index].wrapping_add(1);
            totals.live_bytes[index] = totals.live_bytes[index].wrapping_add(size);
            swept.allocated += aligned;
            if let Some(start) = run_start.take() {
                // SAFETY: the run lies inside this page's payload and every
                // byte of it belonged to dead cells or fillers.
                unsafe {
                    std::ptr::write(
                        page.add(start).cast::<GcHeader>(),
                        GcHeader::new_free(run_bytes as u32),
                    );
                }
                swept
                    .free_ranges
                    .push((page_offset + start as u32, std::mem::take(&mut run_bytes)));
            }
        } else {
            // Reclaim exactly once: a second drop would double-free what
            // the payload owns. Fillers own nothing.
            if !filler && unsafe { !(*h).is_swept() } {
                record(h, &mut swept, totals);
            }
            run_start.get_or_insert(offset);
            run_bytes += aligned;
        }
        offset += aligned;
    }
    if !swept.dead {
        if let Some(start) = run_start {
            // The final dead run touches the bump cursor: it returns to
            // bump allocation.
            debug_assert_eq!(start + run_bytes, cursor);
            swept.bump_cursor = start;
        }
        totals.retained = totals.retained.saturating_add(swept.allocated as u64);
    }
    swept
}
