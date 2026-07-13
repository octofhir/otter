//! Heap spaces: young (semispace), old (free-list), large object.
//!
//! # Contents
//!
//! - [`NewSpace`] — pair of semispaces with bump alloc in
//!   from-space, flipped on scavenge.
//! - [`OldSpace`] — list of pages plus a size-classed free list over
//!   swept holes; allocation reuses holes first, then bumps, then
//!   grows; pages get marked-and-swept on a full GC.
//! - [`LargeObjectSpace`] — one page per oversized allocation.
//!
//! # Invariants
//!
//! - `NewSpace::flip()` swaps `from`/`to` *and* clears the
//!   freshly-from-space pages so the next mutator alloc sees a
//!   pristine bump cursor.
//! - Promotion (young → old) happens inside the scavenger when
//!   `survival_age >= PROMOTE_AFTER_SURVIVALS`. New-space pages
//!   carry the survival counter; old-space pages do not.
//! - The active old-space page (head of `pages`) is the only one
//!   bump-allocated into; older pages are sealed.
//!
//! # See also
//!
//! - GC architecture plan §2.3 (NewSpace / OldSpace / LOS rows).

use crate::compressed::RawGc;
use crate::oom::OutOfMemory;
use crate::page::{CELL_SIZE, PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE};
use crate::page::{LARGE_OBJECT_THRESHOLD, Page, SpaceKind, align_up};

/// Young-gen pages per semispace by default. With 256 KiB pages
/// this gives a 4 MiB nursery (matches NF1 budget).
pub const DEFAULT_NEW_SPACE_PAGES: usize = 16;

/// Cap on new-space pages — guard against runaway growth.
pub const MAX_NEW_SPACE_PAGES: usize = 64;

/// Two-semispace young generation.
pub struct NewSpace {
    from: Vec<Page>,
    to: Vec<Page>,
    /// Page index inside `from` currently being bump-allocated.
    active: usize,
    /// Maximum pages each semispace may grow to.
    max_pages: usize,
}

impl NewSpace {
    /// Create a fresh new-space with `initial_pages` pages on
    /// each side.
    pub fn new(initial_pages: usize) -> Result<Self, OutOfMemory> {
        // The nursery STARTS at `initial_pages` and GROWS up to
        // `MAX_NEW_SPACE_PAGES`. The previous `clamp(initial, DEFAULT,
        // MAX)` set the cap *equal to* the initial size (16), so the
        // semispace could never grow — every nursery-sized live set
        // deadlocked the copying scavenger (no room freed) and fell
        // back to old-space overflow on every alloc. Cap at the real
        // maximum; only the starting page count comes from the arg.
        let max_pages = MAX_NEW_SPACE_PAGES;
        let initial_pages = initial_pages.clamp(1, max_pages);
        let mut from = Vec::with_capacity(initial_pages);
        let mut to = Vec::with_capacity(initial_pages);
        for _ in 0..initial_pages {
            from.push(Page::new(SpaceKind::NewFrom).ok_or(OutOfMemory::CageExhausted)?);
            to.push(Page::new(SpaceKind::NewTo).ok_or(OutOfMemory::CageExhausted)?);
        }
        Ok(Self {
            from,
            to,
            active: 0,
            max_pages,
        })
    }

    /// Bump-allocate `size_aligned` bytes in from-space. Returns
    /// `None` only when every from-space page is full *and* we
    /// have no quota for an extra page — that is the scavenge
    /// trigger.
    pub fn alloc(&mut self, size_aligned: usize) -> Option<u32> {
        loop {
            if self.active < self.from.len() {
                let page = &self.from[self.active];
                if let Some(offset) = page.bump_alloc(size_aligned) {
                    return Some(offset);
                }
                self.active += 1;
                continue;
            }
            // No bump slot in current pages; grow if quota allows.
            if self.from.len() < self.max_pages {
                let extra_from = Page::new(SpaceKind::NewFrom)?;
                let extra_to = Page::new(SpaceKind::NewTo)?;
                self.from.push(extra_from);
                self.to.push(extra_to);
                continue;
            }
            return None;
        }
    }

    /// Bump-allocate inside `to`-space — used by the scavenger
    /// while evacuating survivors.
    pub fn alloc_in_to(&mut self, size_aligned: usize) -> Option<u32> {
        for page in &self.to {
            if let Some(offset) = page.bump_alloc(size_aligned) {
                return Some(offset);
            }
        }
        // To-space ran out — would need to promote remainder to
        // old-gen. The scavenger handles that path; this fn
        // returns None here.
        None
    }

    /// True if we hold no allocation room in `from` without
    /// growing past the cap.
    pub fn from_full(&self) -> bool {
        self.active >= self.from.len() && self.from.len() >= self.max_pages
    }

    /// Iterate over the from-space pages.
    pub fn from_pages(&self) -> &[Page] {
        &self.from
    }

    /// Mutable view of from-space pages.
    pub fn from_pages_mut(&mut self) -> &mut [Page] {
        &mut self.from
    }

    /// Iterate over the to-space pages.
    pub fn to_pages(&self) -> &[Page] {
        &self.to
    }

    /// Number of from-space pages.
    pub fn from_page_count(&self) -> usize {
        self.from.len()
    }

    /// Bytes allocated in from-space across all pages.
    pub fn allocated_bytes(&self) -> usize {
        self.from.iter().map(|p| p.header().allocated_bytes).sum()
    }

    /// Flip semantic: swap `from` ↔ `to`, reset the new from-space
    /// bump cursors and active index. Called by the scavenger
    /// after evacuating survivors and (optionally) after the
    /// caller has fixed up external roots.
    pub fn flip(&mut self) {
        std::mem::swap(&mut self.from, &mut self.to);
        for page in &self.from {
            page.set_space(SpaceKind::NewFrom);
        }
        for page in &self.to {
            page.set_space(SpaceKind::NewTo);
            page.reset_bump();
        }
        self.active = 0;
    }
}

/// Size-class boundaries for the old-space free list. Entry `i` holds
/// free ranges with `size >= FREE_LIST_CLASSES[i]` and (for `i + 1 <
/// FREE_LIST_CLASS_COUNT`) `size < FREE_LIST_CLASSES[i + 1]` — the
/// V8-style category split that keeps first-fit searches short.
const FREE_LIST_CLASSES: [usize; FREE_LIST_CLASS_COUNT] =
    [32, 64, 128, 256, 512, 1024, 4096, 16384];
/// Number of free-list size classes.
const FREE_LIST_CLASS_COUNT: usize = 8;
/// Free ranges smaller than the smallest class stay pure fillers —
/// walkable but never handed back out (their bookkeeping would cost
/// more than the bytes recovered).
const FREE_LIST_MIN_BYTES: usize = FREE_LIST_CLASSES[0];

/// One reusable free range inside an old-space page: the cage offset of
/// its `FREE_TAG` filler header and the total byte length it covers.
#[derive(Debug, Clone, Copy)]
struct FreeEntry {
    offset: u32,
    size: u32,
}

/// Size-classed free list over the reclaimed ranges of old-space pages.
///
/// Rebuilt from scratch by every full-GC sweep (entries never dangle
/// across page reaps) and consumed by [`OldSpace::alloc`] — both mutator
/// old-space allocation and scavenger promotion, so a churn workload
/// reuses the holes its garbage leaves behind instead of growing the
/// page set until the cage exhausts.
#[derive(Default)]
struct OldFreeList {
    bins: [Vec<FreeEntry>; FREE_LIST_CLASS_COUNT],
}

impl OldFreeList {
    fn class_of(size: usize) -> Option<usize> {
        if size < FREE_LIST_MIN_BYTES {
            return None;
        }
        Some(
            FREE_LIST_CLASSES
                .iter()
                .rposition(|&floor| size >= floor)
                .unwrap_or(0),
        )
    }

    fn clear(&mut self) {
        for bin in &mut self.bins {
            bin.clear();
        }
    }

    fn push(&mut self, offset: u32, size: usize) {
        if let Some(class) = Self::class_of(size) {
            self.bins[class].push(FreeEntry {
                offset,
                size: size as u32,
            });
        }
    }

    /// Pop the first range that fits `size_aligned`, searching the class
    /// that guarantees a fit first and falling back to a first-fit scan
    /// of the exact class below it.
    fn take(&mut self, size_aligned: usize) -> Option<FreeEntry> {
        // Requests below the smallest class still search from class 0 —
        // every listed range is at least `FREE_LIST_MIN_BYTES`.
        let start = Self::class_of(size_aligned.max(FREE_LIST_MIN_BYTES))?;
        // Classes above `start` hold only ranges >= their floor > size.
        for class in (start + 1)..FREE_LIST_CLASS_COUNT {
            if let Some(entry) = self.bins[class].pop() {
                return Some(entry);
            }
        }
        // The exact class may hold both fitting and too-small ranges.
        let bin = &mut self.bins[start];
        let index = bin
            .iter()
            .position(|entry| entry.size as usize >= size_aligned)?;
        Some(bin.swap_remove(index))
    }
}

/// Old-generation space: a list of pages plus a size-classed free list
/// over swept holes. Allocation takes a fitting free range first, then
/// bumps in the newest page, then grows by one page.
pub struct OldSpace {
    pages: Vec<Page>,
    free_list: OldFreeList,
    /// Empty pages reserved up front by the scavenger so promotion can
    /// never hit cage exhaustion after the first forwarding write. Held
    /// OUTSIDE `pages` and drawn one at a time only when neither the free
    /// list nor an existing page can serve an allocation — keeping them
    /// out of the bump scan preserves the partially-filled tail page as
    /// the primary bump target across scavenges (appending reserves to
    /// `pages` used to bury it behind fresh empties, leaking one
    /// near-empty page per scavenge).
    standby: Vec<Page>,
}

impl OldSpace {
    /// Empty old-space; pages are added lazily as old-gen alloc
    /// demand arrives.
    pub fn new() -> Self {
        Self {
            pages: Vec::new(),
            free_list: OldFreeList::default(),
            standby: Vec::new(),
        }
    }

    /// Allocate `size_aligned` bytes in old-space: reuse a swept free
    /// range when one fits, otherwise bump, otherwise grow by one page.
    pub fn alloc(&mut self, size_aligned: usize) -> Result<u32, OutOfMemory> {
        if size_aligned > PAGE_PAYLOAD_SIZE {
            return Err(OutOfMemory::AllocationTooLarge {
                requested_bytes: size_aligned as u64,
                max_bytes: PAGE_PAYLOAD_SIZE as u64,
            });
        }
        if let Some(entry) = self.free_list.take(size_aligned) {
            let remainder = entry.size as usize - size_aligned;
            debug_assert!(remainder == 0 || remainder >= CELL_SIZE);
            if remainder > 0 {
                // Cap the tail with a fresh filler so the page's linear
                // header walk stays intact, and hand it back to the list
                // when it is still worth reusing.
                let tail_offset = entry.offset + size_aligned as u32;
                let tail_ptr = crate::page::page_base_from_offset(tail_offset);
                let in_page = tail_offset as usize & (crate::page::PAGE_SIZE - 1);
                // SAFETY: the tail lies inside the same live old page the
                // free entry covered; fillers are never traced.
                unsafe {
                    let header_ptr = tail_ptr.add(in_page) as *mut crate::header::GcHeader;
                    std::ptr::write(
                        header_ptr,
                        crate::header::GcHeader::new_free(remainder as u32),
                    );
                }
                self.free_list.push(tail_offset, remainder);
            }
            // SAFETY: the entry names an in-cage old page carved by this
            // space; account the reused bytes on that page.
            let page_header = unsafe {
                &mut *(crate::page::page_base_from_offset(entry.offset)
                    as *mut crate::page::PageHeader)
            };
            page_header.allocated_bytes += size_aligned;
            return Ok(entry.offset);
        }
        for page in self.pages.iter().rev() {
            if let Some(offset) = page.bump_alloc(size_aligned) {
                return Ok(offset);
            }
        }
        let page = match self.standby.pop() {
            Some(reserved) => reserved,
            None => Page::new(SpaceKind::Old).ok_or(OutOfMemory::CageExhausted)?,
        };
        let offset = page
            .bump_alloc(size_aligned)
            .ok_or(OutOfMemory::AllocationTooLarge {
                requested_bytes: size_aligned as u64,
                max_bytes: PAGE_PAYLOAD_SIZE as u64,
            })?;
        self.pages.push(page);
        Ok(offset)
    }

    /// Drop every free-list entry; the sweep that follows rebuilds the
    /// list from the pages that survive it.
    pub(crate) fn clear_free_list(&mut self) {
        self.free_list.clear();
    }

    /// Record one swept free range (already capped by a `FREE_TAG`
    /// filler header) for reuse.
    pub(crate) fn push_free_range(&mut self, offset: u32, size: usize) {
        self.free_list.push(offset, size);
    }


    /// Atomically reserve empty standby pages before a copying collection.
    ///
    /// Pages are first acquired into a temporary vector. If the cage cannot
    /// satisfy the complete request, that vector drops and the old space
    /// stays unchanged. Reserved pages enter [`Self::alloc`]'s rotation only
    /// when neither the free list nor an existing page has room.
    pub(crate) fn reserve_promotion_pages(&mut self, count: usize) -> Result<(), OutOfMemory> {
        let reserved = Page::new_many(SpaceKind::Old, count).ok_or(OutOfMemory::CageExhausted)?;
        self.standby.extend(reserved);
        Ok(())
    }

    /// Drop every unused standby page back to the cage after a scavenge.
    pub(crate) fn release_unused_promotion_pages(&mut self) {
        self.standby.clear();
    }

    /// Total old-space pages.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Iterate over old-space pages.
    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// Mutable view.
    pub fn pages_mut(&mut self) -> &mut [Page] {
        &mut self.pages
    }

    /// Bytes allocated in old-space (sum of per-page).
    pub fn allocated_bytes(&self) -> usize {
        self.pages.iter().map(|p| p.header().allocated_bytes).sum()
    }

    /// Clear the live-bytes counter on every page so a fresh
    /// mark phase can accumulate.
    pub fn reset_live_bytes(&mut self) {
        for page in &self.pages {
            page.header_mut().live_bytes = 0;
        }
    }

    /// Drop pages whose `live_bytes` is zero after a sweep.
    /// Returns the number of pages reclaimed.
    pub fn reap_dead_pages(&mut self) -> usize {
        let before = self.pages.len();
        self.pages.retain(|p| p.header().live_bytes > 0);
        before - self.pages.len()
    }
}

impl Default for OldSpace {
    fn default() -> Self {
        Self::new()
    }
}

/// Large-object space: one page per oversized allocation.
/// Allocations whose total size exceeds [`LARGE_OBJECT_THRESHOLD`]
/// land here regardless of generation.
pub struct LargeObjectSpace {
    pages: Vec<Page>,
}

impl LargeObjectSpace {
    /// Empty LOS.
    pub fn new() -> Self {
        Self { pages: Vec::new() }
    }

    /// Allocate one page for `size_aligned` bytes (caller has
    /// already aligned). Returns the cage offset of the payload
    /// (header position). The page is dedicated to this single
    /// allocation.
    pub fn alloc(&mut self, size_aligned: usize) -> Result<u32, OutOfMemory> {
        debug_assert!(size_aligned > LARGE_OBJECT_THRESHOLD);
        if size_aligned > PAGE_PAYLOAD_SIZE {
            return Err(OutOfMemory::AllocationTooLarge {
                requested_bytes: size_aligned as u64,
                max_bytes: PAGE_PAYLOAD_SIZE as u64,
            });
        }
        let page = Page::new(SpaceKind::Large).ok_or(OutOfMemory::CageExhausted)?;
        let offset = page
            .bump_alloc(size_aligned)
            .ok_or(OutOfMemory::AllocationTooLarge {
                requested_bytes: size_aligned as u64,
                max_bytes: PAGE_PAYLOAD_SIZE as u64,
            })?;
        self.pages.push(page);
        Ok(offset)
    }

    /// Total LOS pages.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Iterate LOS pages.
    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// Bytes allocated across all LOS pages. One LOS page holds
    /// exactly one object whose size is the page header's
    /// `allocated_bytes`, so this is just a sum.
    pub fn allocated_bytes(&self) -> usize {
        self.pages.iter().map(|p| p.header().allocated_bytes).sum()
    }

    /// Drop LOS pages whose object is unreachable after the
    /// mark phase.
    pub fn reap_dead_pages(&mut self) -> usize {
        let before = self.pages.len();
        self.pages.retain(|p| p.header().live_bytes > 0);
        before - self.pages.len()
    }

    /// Reset live-bytes for next mark cycle.
    pub fn reset_live_bytes(&mut self) {
        for page in &self.pages {
            page.header_mut().live_bytes = 0;
        }
    }
}

impl Default for LargeObjectSpace {
    fn default() -> Self {
        Self::new()
    }
}

/// Round an allocation size (header + payload) up to the cell
/// boundary, with a minimum of one cell so every allocation
/// has at least the forwarding-pointer's worth of payload.
#[inline]
pub fn align_alloc_size(total_bytes: usize) -> usize {
    align_up(total_bytes, CELL_SIZE).max(CELL_SIZE)
}

/// Slot type alias used throughout the GC for clarity — every
/// trace function and barrier sees `*mut RawGc`.
pub type Slot = *mut RawGc;

// Use PAGE_HEADER_SIZE so the import is not flagged as unused.
const _: usize = PAGE_HEADER_SIZE;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressed::{CAGE_TEST_LOCK, Cage};

    fn ensure_cage() {
        let _ = Cage::ensure_default();
    }

    #[test]
    fn old_alloc_reuses_swept_free_ranges() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        ensure_cage();
        let mut old = OldSpace::new();
        // Bump twice to establish a page, then hand the first range back
        // through the free list (as the sweeper would after covering it
        // with a filler header).
        let first = old.alloc(128).expect("first alloc");
        let _second = old.alloc(128).expect("second alloc");
        let pages_before = old.page_count();
        // SAFETY: `first` names live old-page storage this test owns.
        unsafe {
            let ptr = crate::page::page_base_from_offset(first)
                .add(first as usize & (crate::page::PAGE_SIZE - 1))
                as *mut crate::header::GcHeader;
            std::ptr::write(ptr, crate::header::GcHeader::new_free(128));
        }
        old.push_free_range(first, 128);
        // A fitting allocation reuses the range instead of bumping.
        let reused = old.alloc(64).expect("reused alloc");
        assert_eq!(reused, first, "free-list range must be reused");
        assert_eq!(old.page_count(), pages_before);
        // The split tail was re-listed and serves the next fit.
        let tail = old.alloc(64).expect("tail alloc");
        assert_eq!(tail, first + 64, "split tail must be reused next");
    }

    #[test]
    fn standby_pages_do_not_bury_the_bump_tail() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        ensure_cage();
        let mut old = OldSpace::new();
        let first = old.alloc(64).expect("first alloc");
        let pages_after_first = old.page_count();
        // Reserving standby pages must not enter the bump rotation…
        old.reserve_promotion_pages(4).expect("reserve");
        assert_eq!(old.page_count(), pages_after_first);
        // …so the next allocation keeps filling the same tail page.
        let second = old.alloc(64).expect("second alloc");
        assert_eq!(second, first + 64, "tail page keeps filling");
        old.release_unused_promotion_pages();
        assert_eq!(old.page_count(), pages_after_first);
    }

    #[test]
    fn oversized_large_object_returns_error() {
        let mut space = LargeObjectSpace::new();
        let requested = PAGE_PAYLOAD_SIZE + CELL_SIZE;
        assert_eq!(
            space.alloc(requested),
            Err(OutOfMemory::AllocationTooLarge {
                requested_bytes: requested as u64,
                max_bytes: PAGE_PAYLOAD_SIZE as u64,
            })
        );
    }
}
