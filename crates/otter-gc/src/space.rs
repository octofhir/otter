//! Heap spaces: young (semispace), old (free-list), large object.
//!
//! # Contents
//!
//! - [`NewSpace`] — pair of semispaces with bump alloc in
//!   from-space, flipped on scavenge.
//! - [`OldSpace`] — list of pages plus a size-classed free list over
//!   swept holes; allocation reuses holes first, then bumps, then
//!   grows; pages get marked-and-swept on a full GC.
//! - [`LargeObjectSpace`] — one region per oversized allocation,
//!   spanning as many consecutive pages as the body needs.
//!
//! # Invariants
//!
//! - `NewSpace::flip()` swaps `from`/`to` *and* clears the
//!   freshly-from-space pages so the next mutator alloc sees a
//!   pristine bump cursor.
//! - Promotion (young → old) happens inside the scavenger when
//!   `survival_age >= PROMOTE_AFTER_SURVIVALS`. New-space pages
//!   carry the survival counter; old-space pages do not.
//! - Old allocation reuses swept holes, then available page tails. Promotion
//!   preflight guarantees capacity before the first forwarding write; existing
//!   contiguous tail space can satisfy that guarantee without standby pages.
//!
//! # See also
//!
//! - GC architecture plan §2.3 (NewSpace / OldSpace / LOS rows).

use crate::compressed::RawGc;
use crate::oom::OutOfMemory;
use crate::page::{CELL_SIZE, PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE};
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

    /// Current mutator page for a generated-code allocation probe.
    ///
    /// The returned page remains cage-owned for the heap lifetime. Generated
    /// code must still validate its live [`SpaceKind`] and bump limit before
    /// carving a cell because a collection may flip the semispaces after this
    /// view is captured.
    pub(crate) fn machine_active_page(&self) -> Option<&Page> {
        self.from.get(self.active)
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
/// FREE_LIST_CLASS_COUNT`) `size < FREE_LIST_CLASSES[i + 1]`.
///
/// The split is V8's `FreeListMany::categories_min`: one class per
/// [`CELL_SIZE`] step below [`FREE_LIST_DOUBLING_MIN_BYTES`], then one per
/// doubling. Every range in a precise class has exactly the class size, so
/// a request there pops in O(1); a request in a doubling class pops the
/// largest range of its class or fails in O(1), and any class above the
/// request's own guarantees a fit. No allocation ever walks a class
/// linearly, so a mutator that allocates into a swept heap cannot degrade
/// with the number of holes the sweep left behind.
const FREE_LIST_CLASSES: [usize; FREE_LIST_CLASS_COUNT] = free_list_classes();
/// Floor of the first doubling class; every smaller size has a precise
/// class of its own.
const FREE_LIST_DOUBLING_MIN_BYTES: usize = 256;
/// Number of precise classes: `32, 40, ..., 248`.
const FREE_LIST_PRECISE_COUNT: usize =
    (FREE_LIST_DOUBLING_MIN_BYTES - FREE_LIST_MIN_BYTES) / CELL_SIZE;
/// Number of doubling classes: `256, 512, ..., 131072`.
const FREE_LIST_DOUBLING_COUNT: usize = 10;
/// Number of free-list size classes.
const FREE_LIST_CLASS_COUNT: usize = FREE_LIST_PRECISE_COUNT + FREE_LIST_DOUBLING_COUNT;
/// Free ranges smaller than the smallest class stay pure fillers —
/// walkable but never handed back out (their bookkeeping would cost
/// more than the bytes recovered).
const FREE_LIST_MIN_BYTES: usize = 32;

const fn free_list_classes() -> [usize; FREE_LIST_CLASS_COUNT] {
    let mut classes = [0; FREE_LIST_CLASS_COUNT];
    let mut index = 0;
    while index < FREE_LIST_PRECISE_COUNT {
        classes[index] = FREE_LIST_MIN_BYTES + index * CELL_SIZE;
        index += 1;
    }
    let mut floor = FREE_LIST_DOUBLING_MIN_BYTES;
    while index < FREE_LIST_CLASS_COUNT {
        classes[index] = floor;
        floor *= 2;
        index += 1;
    }
    classes
}

/// One reusable free range inside an old-space page: the cage offset of
/// its `FREE_TAG` filler header and the total byte length it covers.
///
/// Ordered by size so a doubling class can hand out its largest range
/// first; the offset breaks ties deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FreeEntry {
    offset: u32,
    size: u32,
}

impl Ord for FreeEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.size, self.offset).cmp(&(other.size, other.offset))
    }
}

impl PartialOrd for FreeEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
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
    /// Precise classes: every entry has exactly the class size.
    precise: [Vec<FreeEntry>; FREE_LIST_PRECISE_COUNT],
    /// Doubling classes: largest range first.
    doubling: [std::collections::BinaryHeap<FreeEntry>; FREE_LIST_DOUBLING_COUNT],
}

impl OldFreeList {
    fn class_of(size: usize) -> Option<usize> {
        if size < FREE_LIST_MIN_BYTES {
            return None;
        }
        if size < FREE_LIST_DOUBLING_MIN_BYTES {
            return Some((size - FREE_LIST_MIN_BYTES) / CELL_SIZE);
        }
        Some(
            FREE_LIST_CLASSES
                .iter()
                .rposition(|&floor| size >= floor)
                .unwrap_or(FREE_LIST_PRECISE_COUNT),
        )
    }

    fn clear(&mut self) {
        for bin in &mut self.precise {
            bin.clear();
        }
        for bin in &mut self.doubling {
            bin.clear();
        }
    }

    fn push(&mut self, offset: u32, size: usize) {
        let Some(class) = Self::class_of(size) else {
            return;
        };
        let entry = FreeEntry {
            offset,
            size: size as u32,
        };
        if class < FREE_LIST_PRECISE_COUNT {
            debug_assert_eq!(size, FREE_LIST_CLASSES[class]);
            self.precise[class].push(entry);
        } else {
            self.doubling[class - FREE_LIST_PRECISE_COUNT].push(entry);
        }
    }

    fn pop_class(&mut self, class: usize) -> Option<FreeEntry> {
        if class < FREE_LIST_PRECISE_COUNT {
            self.precise[class].pop()
        } else {
            self.doubling[class - FREE_LIST_PRECISE_COUNT].pop()
        }
    }

    /// Pop a range that fits `size_aligned`: the request's own class first
    /// (exact for a precise class, largest-first for a doubling class), then
    /// the smallest class above it, whose every range is guaranteed to fit.
    fn take(&mut self, size_aligned: usize) -> Option<FreeEntry> {
        // Requests below the smallest class still search from class 0 —
        // every listed range is at least `FREE_LIST_MIN_BYTES`.
        let start = Self::class_of(size_aligned.max(FREE_LIST_MIN_BYTES))?;
        if start < FREE_LIST_PRECISE_COUNT {
            if let Some(entry) = self.precise[start].pop() {
                return Some(entry);
            }
        } else {
            let bin = &mut self.doubling[start - FREE_LIST_PRECISE_COUNT];
            if bin
                .peek()
                .is_some_and(|largest| largest.size as usize >= size_aligned)
            {
                return bin.pop();
            }
        }
        // Classes above `start` hold only ranges >= their floor > size.
        for class in (start + 1)..FREE_LIST_CLASS_COUNT {
            if let Some(entry) = self.pop_class(class) {
                return Some(entry);
            }
        }
        None
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
            let remainder = (entry.size as usize)
                .checked_sub(size_aligned)
                .expect("a free-list range is only handed out for a request it fits");
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
    ///
    /// `max_promotion_bytes` bounds all bytes that may be promoted, including
    /// descendants. When the newest page can hold that entire bound, its tail
    /// already proves capacity: reserving and immediately releasing empty
    /// pages would only zero memory and call the OS on every tiny scavenge.
    pub(crate) fn reserve_promotion_pages(
        &mut self,
        count: usize,
        max_promotion_bytes: usize,
    ) -> Result<(), OutOfMemory> {
        if self
            .pages
            .last()
            .is_some_and(|page| page.header().bump_remaining() >= max_promotion_bytes)
        {
            return Ok(());
        }
        let reserved = Page::new_many(SpaceKind::Old, count).ok_or(OutOfMemory::CageExhausted)?;
        self.standby.extend(reserved);
        Ok(())
    }

    /// Drop every unused standby page back to the cage after a scavenge.
    pub(crate) fn release_unused_promotion_pages(&mut self) {
        self.standby.clear();
    }

    /// Take ownership of a page whose objects are already live.
    ///
    /// Used by the image restore path, which fills a page from a captured
    /// byte image and relocates its pointers before handing it over. The
    /// free list is untouched: the page arrives fully bump-allocated.
    pub(crate) fn adopt_page(&mut self, page: Page) {
        self.pages.push(page);
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
        // The region is sized to the body, not the other way round: a
        // string or backing store larger than one page spans as many as it
        // needs, the way V8's `LargePage` is a variable-sized chunk. A
        // fixed one-page region would make any body over ~128 KiB
        // unallocatable, which for string storage means unrepresentable.
        let span = size_aligned
            .saturating_add(PAGE_HEADER_SIZE)
            .div_ceil(PAGE_SIZE);
        let span = u32::try_from(span.max(1)).map_err(|_| OutOfMemory::AllocationTooLarge {
            requested_bytes: size_aligned as u64,
            max_bytes: u64::from(u32::MAX) * PAGE_SIZE as u64,
        })?;
        let page = Page::new_spanning(SpaceKind::Large, span).ok_or(OutOfMemory::CageExhausted)?;
        let offset = page
            .bump_alloc(size_aligned)
            .ok_or(OutOfMemory::CageExhausted)?;
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
        old.reserve_promotion_pages(4, PAGE_PAYLOAD_SIZE * 2)
            .expect("reserve");
        assert_eq!(old.page_count(), pages_after_first);
        // …so the next allocation keeps filling the same tail page.
        let second = old.alloc(64).expect("second alloc");
        assert_eq!(second, first + 64, "tail page keeps filling");
        old.release_unused_promotion_pages();
        assert_eq!(old.page_count(), pages_after_first);
    }

    #[test]
    fn promotion_preflight_uses_existing_tail_without_cage_pages() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        ensure_cage();
        let mut old = OldSpace::new();
        let first = old.alloc(64).expect("first alloc");
        let tail = old
            .pages()
            .last()
            .expect("old page")
            .header()
            .bump_remaining();
        let free_before = crate::compressed::cage_stats()
            .expect("cage stats")
            .free_pages;

        old.reserve_promotion_pages(2, tail)
            .expect("tail covers nursery");
        assert!(old.standby.is_empty());
        assert_eq!(
            crate::compressed::cage_stats()
                .expect("cage stats")
                .free_pages,
            free_before
        );
        assert_eq!(old.alloc(tail).expect("promote full bound"), first + 64);
        assert_eq!(old.page_count(), 1);

        old.reserve_promotion_pages(2, CELL_SIZE)
            .expect("exhausted tail reserves");
        assert_eq!(old.standby.len(), 2);
        old.release_unused_promotion_pages();
        assert!(old.standby.is_empty());
    }

    /// A body larger than one page gets a region spanning as many as it
    /// needs. Refusing it would make any string over ~128 KiB
    /// unrepresentable now that a string's characters live in its own
    /// cell rather than in a side buffer.
    #[test]
    fn a_body_larger_than_a_page_spans_several() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let _ = Cage::ensure_default();
        let mut space = LargeObjectSpace::new();
        let requested = PAGE_PAYLOAD_SIZE + CELL_SIZE;
        let offset = space.alloc(requested).expect("spanning region");
        let page = &space.pages()[0];
        assert!(page.span_pages() >= 2, "region must cover several pages");
        assert_eq!(
            offset,
            page.cage_offset() + PAGE_HEADER_SIZE as u32,
            "the body starts right after the first page's header",
        );
        assert!(
            page.header().bump_cursor <= page.header().span_bytes(),
            "the bump cursor stays inside the region",
        );
    }

    /// The region walk sees exactly the one body it holds.
    #[test]
    fn a_spanning_region_walks_to_one_body() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let _ = Cage::ensure_default();
        let mut space = LargeObjectSpace::new();
        let requested = align_up(PAGE_PAYLOAD_SIZE * 2, CELL_SIZE);
        let offset = space.alloc(requested).expect("spanning region");
        let page = &space.pages()[0];
        // SAFETY: the region's single header was just written by the
        // allocation above.
        unsafe {
            let header = crate::page::page_base_from_offset(offset)
                .add(offset as usize & (PAGE_SIZE - 1))
                .cast::<crate::header::GcHeader>();
            std::ptr::write(header, crate::header::GcHeader::new(7, requested as u32));
        }
        let mut seen = 0usize;
        // SAFETY: the one header in the region is initialised above.
        unsafe { page.for_each_object(|_, _| seen += 1) };
        assert_eq!(seen, 1);
    }
}

#[cfg(test)]
mod free_list_tests {
    use super::*;

    #[test]
    fn classes_are_precise_then_doubling() {
        assert_eq!(FREE_LIST_CLASSES[0], FREE_LIST_MIN_BYTES);
        for window in FREE_LIST_CLASSES[..FREE_LIST_PRECISE_COUNT].windows(2) {
            assert_eq!(window[1] - window[0], CELL_SIZE);
        }
        assert_eq!(
            FREE_LIST_CLASSES[FREE_LIST_PRECISE_COUNT - 1] + CELL_SIZE,
            FREE_LIST_DOUBLING_MIN_BYTES
        );
        assert_eq!(
            FREE_LIST_CLASSES[FREE_LIST_PRECISE_COUNT],
            FREE_LIST_DOUBLING_MIN_BYTES
        );
        for window in FREE_LIST_CLASSES[FREE_LIST_PRECISE_COUNT..].windows(2) {
            assert_eq!(window[1], window[0] * 2);
        }
        assert!(FREE_LIST_CLASSES[FREE_LIST_CLASS_COUNT - 1] < PAGE_PAYLOAD_SIZE);
    }

    #[test]
    fn precise_requests_pop_exact_ranges() {
        let mut list = OldFreeList::default();
        list.push(0x1000, 64);
        list.push(0x2000, 72);
        let taken = list.take(64).expect("exact class range");
        assert_eq!((taken.offset, taken.size), (0x1000, 64));
        // A request between precise classes rounds to its own class and
        // otherwise takes the smallest guaranteed-fit class above.
        let taken = list.take(64).expect("range from the class above");
        assert_eq!((taken.offset, taken.size), (0x2000, 72));
        assert!(list.take(64).is_none());
    }

    #[test]
    fn doubling_requests_take_the_largest_range_or_fail_in_place() {
        let mut list = OldFreeList::default();
        list.push(0x1000, 600);
        list.push(0x2000, 900);
        list.push(0x3000, 520);
        // 700 lives in the 512 class: the largest range there fits.
        let taken = list.take(700).expect("largest range of the class");
        assert_eq!((taken.offset, taken.size), (0x2000, 900));
        // Nothing left in the class fits 700; with no class above, fail
        // without scanning.
        assert!(list.take(700).is_none());
        // The remaining ranges still serve smaller requests, largest first.
        let taken = list.take(520).expect("next largest");
        assert_eq!((taken.offset, taken.size), (0x1000, 600));
        // A precise request is served from the class above when its own
        // class is empty.
        let taken = list.take(200).expect("guaranteed fit above");
        assert_eq!((taken.offset, taken.size), (0x3000, 520));
    }

    #[test]
    fn sizes_between_the_last_precise_class_and_512_share_one_range_class() {
        let mut list = OldFreeList::default();
        list.push(0x1000, 256);
        list.push(0x2000, 264);
        // 300 has no precise class: it must never be served by a smaller
        // range, and its own class holds both listed ranges.
        assert!(list.take(300).is_none());
        let taken = list.take(264).expect("largest range of the 256 class");
        assert_eq!((taken.offset, taken.size), (0x2000, 264));
        let taken = list.take(256).expect("exact remaining range");
        assert_eq!((taken.offset, taken.size), (0x1000, 256));
    }

    #[test]
    fn ranges_below_the_minimum_are_never_listed() {
        let mut list = OldFreeList::default();
        list.push(0x1000, FREE_LIST_MIN_BYTES - CELL_SIZE);
        assert!(list.take(CELL_SIZE).is_none());
    }
}
