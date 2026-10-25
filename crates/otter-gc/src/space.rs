//! Heap spaces: young (semispace), old (free-list), large object (old and
//! young).
//!
//! # Contents
//!
//! - [`NewSpace`] — pair of semispaces with bump alloc in
//!   from-space, flipped on scavenge; its capacity starts at
//!   [`DEFAULT_NEW_SPACE_PAGES`] and doubles once the scavenges since its
//!   last growth kept a whole capacity alive.
//! - [`OldSpace`] — list of pages plus a size-classed free list over
//!   swept holes, with intrusive links and a nonempty-class bitmap;
//!   allocation reuses holes first, then bumps, then
//!   grows; pages get marked-and-swept on a full GC.
//! - [`LargeObjectSpace`] — one region per oversized allocation,
//!   spanning as many consecutive pages as the body needs. The young one
//!   holds large bodies the nursery admitted; a scavenge hands survivors'
//!   regions to the old one and releases the rest.
//!
//! # Invariants
//!
//! - `NewSpace::flip()` retains the copied survivors in the new `from`
//!   pages and resets recycled inactive `to` pages. Mutator allocation
//!   continues after the survivors; the next evacuation starts with empty `to`.
//! - Promotion (young → old) happens inside the scavenger for objects
//!   below their page's `age_mark`, i.e. objects that already survived one
//!   scavenge. New-space pages carry the mark; old-space pages do not.
//! - Old allocation reuses swept holes, then available page tails. Promotion
//!   preflight guarantees capacity before the first forwarding write; existing
//!   contiguous tail space can satisfy that guarantee without standby pages.
//! - A semispace never holds more pages than its capacity: allocation past
//!   it is a scavenge, never a growth.
//! - Unused standby pages stay reserved across scavenges, so a steady nursery
//!   cycle neither zeroes nor re-faults promotion pages. A full collection's
//!   page reap returns them to the cage.
//! - A memory-releasing full sweep discards unused nursery payload physically,
//!   preserving page headers, virtual addresses and semispace capacity.
//! - The same request may discard recovered old ranges and unused bump tails;
//!   filler headers and intrusive links retain their OS pages.
//!
//! # See also
//!
//! - GC architecture plan §2.3 (NewSpace / OldSpace / LOS rows).

use crate::compressed::RawGc;
use crate::oom::OutOfMemory;
use crate::page::{CELL_SIZE, PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE};
use crate::page::{LARGE_OBJECT_THRESHOLD, Page, SpaceKind, align_up};

/// Young-gen pages per semispace at creation, its starting capacity. With
/// 256 KiB pages this gives a 4 MiB nursery.
pub const DEFAULT_NEW_SPACE_PAGES: usize = 16;

/// Most pages a semispace's capacity grows to: 32 MiB. A larger nursery
/// gives short-lived objects more allocation to die in before a scavenge
/// copies them.
pub const MAX_NEW_SPACE_PAGES: usize = 128;

/// Two-semispace young generation.
pub struct NewSpace {
    from: Vec<Page>,
    to: Vec<Page>,
    /// Cage offsets of the from-space bodies whose type has a reclamation
    /// hook. A scavenge visits only these to reclaim the dead ones, never
    /// the dead cells around them.
    cleanup: Vec<u32>,
    /// Page index inside `from` currently being bump-allocated.
    active: usize,
    /// Page index inside `to` the scavenger currently copies survivors into.
    /// Pages before it are full for every request: each object is at most
    /// half a page, so a page is left only once it is more than half full.
    to_active: usize,
    /// Pages each semispace holds before a scavenge is due: the nursery's
    /// capacity, doubled by [`Self::grow_capacity`] once enough survives.
    capacity: usize,
    /// Most pages the capacity may grow to.
    max_pages: usize,
}

impl NewSpace {
    /// Create a fresh new-space with `initial_pages` pages on
    /// each side.
    pub fn new(initial_pages: usize) -> Result<Self, OutOfMemory> {
        // The capacity starts at `initial_pages` and grows, after scavenges
        // whose survivors filled it, up to `MAX_NEW_SPACE_PAGES`.
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
            cleanup: Vec::new(),
            active: 0,
            to_active: 0,
            capacity: initial_pages,
            max_pages,
        })
    }

    /// Whether the capacity is already at its largest page count.
    #[must_use]
    pub fn at_maximum(&self) -> bool {
        self.capacity >= self.max_pages
    }

    /// The nursery's capacity in bytes: one semispace's pages.
    #[must_use]
    pub fn capacity_bytes(&self) -> usize {
        self.capacity * crate::page::PAGE_SIZE
    }

    /// Double the capacity, up to its maximum. The pages arrive as
    /// allocation reaches them.
    pub fn grow_capacity(&mut self) {
        self.capacity = (self.capacity * 2).min(self.max_pages);
    }

    /// Track a fresh from-space body whose type has a reclamation hook.
    #[inline]
    pub fn track_cleanup(&mut self, offset: u32) {
        self.cleanup.push(offset);
    }

    /// The tracked from-space bodies, leaving the list empty for a
    /// scavenge to refill with its young survivors.
    pub(crate) fn take_cleanup(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.cleanup)
    }

    /// Whether `offset` is tracked; only debug verification asks.
    #[cfg(debug_assertions)]
    pub(crate) fn tracks_cleanup(&self, offset: u32) -> bool {
        self.cleanup.contains(&offset)
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
            if self.from.len() < self.capacity {
                let extra_from = Page::new(SpaceKind::NewFrom)?;
                let extra_to = Page::new(SpaceKind::NewTo)?;
                self.from.push(extra_from);
                self.to.push(extra_to);
                continue;
            }
            return None;
        }
    }

    /// Hand the unused tail of the current from-space page to a linear
    /// allocation buffer.
    ///
    /// Moves to the next page — growing the semispace while quota allows —
    /// until one has at least `min_bytes` left. Returns the page base
    /// address and the tail's `[start, end)` absolute addresses. The page's
    /// own cursor stays at `start`: the buffer owner publishes its top into
    /// the page, and no other allocation may bump the page meanwhile.
    /// A failed search leaves the prior active cursor unchanged so a
    /// smaller source allocation can still consume its existing tail.
    pub(crate) fn take_tail(&mut self, min_bytes: usize) -> Option<(usize, usize, usize)> {
        let mut active = self.active;
        loop {
            if active < self.from.len() {
                let page = &self.from[active];
                let header = page.header();
                if header.bump_remaining() >= min_bytes {
                    self.active = active;
                    let base = page.base_ptr() as usize;
                    return Some((base, base + header.bump_cursor, base + header.span_bytes()));
                }
                active += 1;
                continue;
            }
            if self.from.len() < self.capacity {
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
        // One moving to-space top: never rescan the pages already filled.
        while let Some(page) = self.to.get(self.to_active) {
            if let Some(offset) = page.bump_alloc(size_aligned) {
                return Some(offset);
            }
            self.to_active += 1;
        }
        // To-space ran out — would need to promote remainder to
        // old-gen. The scavenger handles that path; this fn
        // returns None here.
        None
    }

    /// True if we hold no allocation room in `from` without
    /// growing past the cap.
    pub fn from_full(&self) -> bool {
        self.active >= self.from.len() && self.from.len() >= self.capacity
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

    /// Release unused physical nursery pages after a completed full sweep.
    /// Inactive pages have reset bump cursors; active pages retain every byte
    /// up to their published bump cursor. Inward OS-page rounding also retains
    /// the page-header OS page. No capacity or allocation metadata changes.
    ///
    /// # Safety
    /// The mutator and all marker/scavenger workers must be stopped. All LABs
    /// must be retired, weak processing and reclamation hooks completed, and
    /// no reader may retain a pointer into either inactive payload or active
    /// unused tails. Ordinary minor collections deliberately do not call this.
    pub(crate) unsafe fn discard_unused_payload(&self) -> usize {
        let Some(granule) = crate::compressed::discard::system_page_size() else {
            return 0;
        };
        debug_assert!(self.to.iter().all(|page| {
            page.header().bump_cursor == PAGE_HEADER_SIZE && page.header().allocated_bytes == 0
        }));
        let mut discarded = 0usize;
        for page in self.from.iter().chain(&self.to) {
            let cursor = page.header().bump_cursor;
            debug_assert!(cursor >= PAGE_HEADER_SIZE && cursor <= page.header().span_bytes());
            // SAFETY: the caller has completed all readers and published the
            // cursor. The entire supplied suffix is unused, including on an
            // inactive page whose reset cursor follows its retained header.
            discarded = discarded.saturating_add(unsafe {
                crate::compressed::discard::unused_range(
                    page.base_ptr().add(cursor),
                    page.header().span_bytes() - cursor,
                    granule,
                )
            });
        }
        discarded
    }

    /// Swap `from` ↔ `to`, retaining copied survivors and resetting the
    /// recycled inactive pages and allocation indices. Called by the scavenger
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
        self.to_active = 0;
    }
}

mod free_list;
mod old_discard;
use free_list::{FreeEntry, OldFreeList};

/// Old-generation space: a list of pages plus a size-classed free list
/// over swept holes and retired page tails. Allocation takes a fitting free
/// range first, then bumps in one of the few pages that still have room,
/// then grows by one page; it never walks the full page list.
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
    /// near-empty page per scavenge). Unused pages stay here for the next
    /// scavenge and return to the cage when a full collection reaps pages.
    standby: Vec<Page>,
    /// Cage offsets of the pages that still offer bump room, oldest first.
    /// A page leaves the set once its tail falls below
    /// [`OPEN_PAGE_MIN_ROOM`]; that tail joins the free list. Allocation
    /// therefore probes only pages with real room instead of every page.
    open: Vec<u32>,
    /// Linear allocation area carved from one free-list range: allocations
    /// bump through it, so the free list is consulted once per range rather
    /// than split and re-inserted per object. Its unused part always starts
    /// with a free filler header, keeping the page walkable.
    lab: Option<FreeEntry>,
    /// Sum of every page's `allocated_bytes`: the bytes cells occupy,
    /// maintained by every allocation and return and recounted after a sweep.
    allocated: usize,
}

/// Bump room below which an old page closes: its tail becomes one free-list
/// range and the page stops being probed for bump allocation.
const OPEN_PAGE_MIN_ROOM: usize = 2048;

impl OldSpace {
    /// Empty old-space; pages are added lazily as old-gen alloc
    /// demand arrives.
    pub fn new() -> Self {
        Self {
            pages: Vec::new(),
            free_list: OldFreeList::default(),
            standby: Vec::new(),
            open: Vec::new(),
            lab: None,
            allocated: 0,
        }
    }

    /// Allocate `size_aligned` bytes in old-space: reuse a swept free
    /// range when one fits, otherwise bump, otherwise grow by one page.
    pub fn alloc(&mut self, size_aligned: usize) -> Result<u32, OutOfMemory> {
        let offset = self.alloc_inner(size_aligned)?;
        self.allocated += size_aligned;
        Ok(offset)
    }

    fn alloc_inner(&mut self, size_aligned: usize) -> Result<u32, OutOfMemory> {
        if size_aligned > PAGE_PAYLOAD_SIZE {
            return Err(OutOfMemory::AllocationTooLarge {
                requested_bytes: size_aligned as u64,
                max_bytes: PAGE_PAYLOAD_SIZE as u64,
            });
        }
        let range = match self.lab.take() {
            Some(lab) if lab.size as usize >= size_aligned => Some(lab),
            // An exhausted area's rest is already a filler-capped range.
            Some(lab) => {
                // SAFETY: the exclusive LAB remainder is filler-capped in a
                // live page owned by this space; it was never listed.
                unsafe { self.free_list.push(lab.offset, lab.size as usize) };
                self.free_list.take(size_aligned)
            }
            None => self.free_list.take(size_aligned),
        };
        if let Some(entry) = range {
            let remainder = (entry.size as usize)
                .checked_sub(size_aligned)
                .expect("a free-list range is only handed out for a request it fits");
            debug_assert!(remainder == 0 || remainder >= CELL_SIZE);
            if remainder > 0 {
                // Cap the rest with a fresh filler so the page's linear
                // header walk stays intact; it stays the linear area.
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
                self.lab = Some(FreeEntry {
                    offset: tail_offset,
                    size: remainder as u32,
                });
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
        // Probe only pages that still have bump room, newest first. A page
        // whose room drops below `OPEN_PAGE_MIN_ROOM` hands its tail to the
        // free list and closes, so a full heap never costs a page scan.
        let mut index = self.open.len();
        while index > 0 {
            index -= 1;
            let page_offset = self.open[index];
            // SAFETY: `open` names only live old pages owned by this space;
            // `reap_dead_pages` drops the offsets of every page it releases.
            let header = unsafe {
                &mut *(crate::page::page_base_from_offset(page_offset)
                    as *mut crate::page::PageHeader)
            };
            let cursor = header.bump_cursor;
            if cursor + size_aligned <= header.span_bytes() {
                header.bump_cursor = cursor + size_aligned;
                header.allocated_bytes += size_aligned;
                return Ok(page_offset + cursor as u32);
            }
            if header.span_bytes() - cursor < OPEN_PAGE_MIN_ROOM {
                Self::retire_bump_tail(&mut self.free_list, page_offset, header);
                self.open.swap_remove(index);
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
        self.open.push(page.cage_offset());
        self.pages.push(page);
        Ok(offset)
    }

    /// A linear area of at least `min` and at most `max` bytes. Swept holes
    /// come first, as for [`Self::alloc`]: one of at least `want` bytes,
    /// else one that fits `min`; then the newest open page's bump tail; then
    /// a fresh page. A hole, tail or page larger than `max` gives `max` and
    /// keeps the rest for later requests. The bytes count as allocated until
    /// [`Self::return_lab`] gives back the unused rest. Nothing here walks
    /// the page list.
    pub(crate) fn take_lab(
        &mut self,
        min: usize,
        want: usize,
        max: usize,
    ) -> Result<(u32, usize), OutOfMemory> {
        debug_assert!(min <= want && want <= max && max <= PAGE_PAYLOAD_SIZE);
        let (offset, len) = self.take_lab_inner(min, want, max)?;
        self.allocated += len;
        Ok((offset, len))
    }

    fn take_lab_inner(
        &mut self,
        min: usize,
        want: usize,
        max: usize,
    ) -> Result<(u32, usize), OutOfMemory> {
        if let Some(entry) = self
            .free_list
            .take(want)
            .or_else(|| self.free_list.take(min))
        {
            let len = (entry.size as usize).min(max);
            let rest = entry.size as usize - len;
            if rest != 0 {
                let rest_offset = entry.offset + len as u32;
                let in_page = rest_offset as usize & (PAGE_SIZE - 1);
                // SAFETY: the rest lies inside the same filler-capped range
                // of a live page of this space; capping it keeps the page
                // walkable and lists it again.
                unsafe {
                    std::ptr::write(
                        crate::page::page_base_from_offset(rest_offset)
                            .add(in_page)
                            .cast::<crate::header::GcHeader>(),
                        crate::header::GcHeader::new_free(rest as u32),
                    );
                    self.free_list.push(rest_offset, rest);
                }
            }
            // SAFETY: the entry names a range of a live page of this space;
            // account the handed-out bytes on that page.
            let page_header = unsafe {
                &mut *(crate::page::page_base_from_offset(entry.offset)
                    as *mut crate::page::PageHeader)
            };
            page_header.allocated_bytes += len;
            return Ok((entry.offset, len));
        }
        while let Some(&page_offset) = self.open.last() {
            // SAFETY: `open` names only live old pages owned by this space.
            let header = unsafe {
                &mut *(crate::page::page_base_from_offset(page_offset)
                    as *mut crate::page::PageHeader)
            };
            let cursor = header.bump_cursor;
            let room = header.span_bytes() - cursor;
            if room >= min {
                let len = room.min(max);
                header.bump_cursor = cursor + len;
                header.allocated_bytes += len;
                if header.span_bytes() - header.bump_cursor < OPEN_PAGE_MIN_ROOM {
                    Self::retire_bump_tail(&mut self.free_list, page_offset, header);
                    self.open.pop();
                }
                return Ok((page_offset + cursor as u32, len));
            }
            Self::retire_bump_tail(&mut self.free_list, page_offset, header);
            self.open.pop();
        }
        let page = match self.standby.pop() {
            Some(reserved) => reserved,
            None => Page::new(SpaceKind::Old).ok_or(OutOfMemory::CageExhausted)?,
        };
        let len = page.header().bump_remaining().min(max);
        let offset = page
            .bump_alloc(len)
            .ok_or(OutOfMemory::AllocationTooLarge {
                requested_bytes: min as u64,
                max_bytes: PAGE_PAYLOAD_SIZE as u64,
            })?;
        if page.header().bump_remaining() >= OPEN_PAGE_MIN_ROOM {
            self.open.push(page.cage_offset());
        }
        self.pages.push(page);
        Ok((offset, len))
    }

    /// Return the unused `size` bytes at `offset` of an area
    /// [`Self::take_lab`] or [`Self::alloc`] handed out: cap them with a free
    /// filler so the page stays walkable and list them for reuse.
    pub(crate) fn return_lab(&mut self, offset: u32, size: usize) {
        if size == 0 {
            return;
        }
        let in_page = offset as usize & (PAGE_SIZE - 1);
        let base = crate::page::page_base_from_offset(offset);
        // SAFETY: the range is the unallocated tail of an area this space
        // carved from one of its live pages; no object was written there.
        unsafe {
            std::ptr::write(
                base.add(in_page).cast::<crate::header::GcHeader>(),
                crate::header::GcHeader::new_free(size as u32),
            );
            let page_header = &mut *(base as *mut crate::page::PageHeader);
            page_header.allocated_bytes -= size;
            self.free_list.push(offset, size);
        }
        self.allocated -= size;
    }

    /// Close a page's bump area: cap the unused tail with a free filler so the
    /// page's linear header walk stays intact, hand the tail to the free
    /// list, and move the cursor to the end.
    fn retire_bump_tail(
        free_list: &mut OldFreeList,
        page_offset: u32,
        header: &mut crate::page::PageHeader,
    ) {
        let end = header.span_bytes();
        let cursor = header.bump_cursor;
        if cursor >= end {
            return;
        }
        let remaining = end - cursor;
        let tail_offset = page_offset + cursor as u32;
        // SAFETY: `[cursor, end)` is the page's unallocated tail inside its
        // own live mapping; fillers are never traced.
        unsafe {
            let header_ptr = crate::page::page_base_from_offset(tail_offset).add(cursor)
                as *mut crate::header::GcHeader;
            std::ptr::write(
                header_ptr,
                crate::header::GcHeader::new_free(remaining as u32),
            );
        }
        header.bump_cursor = end;
        // SAFETY: the page tail was just capped above, is exclusively
        // owned by this space and stays live until the list is cleared.
        unsafe { free_list.push(tail_offset, remaining) };
    }

    /// Close every page to allocation for a sweep: no free range and no bump
    /// room remains until [`Self::apply_swept_page`] reopens a page with
    /// what its walk found. Pages taken from here on are fresh. Until then a
    /// page counts the bytes its mark found live.
    pub(crate) fn begin_sweep(&mut self) {
        self.clear_free_list();
        self.open.clear();
        for page in &self.pages {
            let header = page.header_mut();
            header.allocated_bytes = header.live_bytes.min(header.allocated_bytes);
        }
        self.recount_allocated();
    }

    /// Give the walked page at `base` its new bump cursor and allocated
    /// bytes, and reopen its bump room when real room remains. Its free
    /// ranges are listed separately, after its dead cells' hooks ran.
    pub(crate) fn apply_swept_page(&mut self, base: usize, bump_cursor: usize, allocated: usize) {
        // SAFETY: `base` is a live old page of this space whose walk ended;
        // only this thread writes its header from here on.
        let header = unsafe { &mut *(base as *mut crate::page::PageHeader) };
        self.allocated = self.allocated - header.allocated_bytes + allocated;
        header.allocated_bytes = allocated;
        header.bump_cursor = bump_cursor;
        if header.span_bytes() - bump_cursor >= OPEN_PAGE_MIN_ROOM {
            self.open.push(header.cage_offset);
        }
    }

    /// Whether an allocation can reuse space without taking a fresh page:
    /// a listed range, the linear area or a page with bump room.
    pub(crate) fn has_reusable_space(&self) -> bool {
        self.lab.is_some() || !self.free_list.is_empty() || !self.open.is_empty()
    }

    /// Account the walked page at `base` as holding nothing. It stays closed
    /// to allocation until [`Self::reap_swept_pages`] returns it.
    pub(crate) fn apply_dead_page(&mut self, base: usize) {
        // SAFETY: as in `apply_swept_page`.
        let header = unsafe { &mut *(base as *mut crate::page::PageHeader) };
        self.allocated -= header.allocated_bytes;
        header.allocated_bytes = 0;
    }

    /// Return the pages a sweep found without survivors, and every unused
    /// standby page, to the cage. No free range lies in them: their walks
    /// listed none and the sweep closed every page first.
    pub(crate) fn reap_swept_pages(&mut self, dead: &[usize]) -> usize {
        let before = self.pages.len();
        let mut dead = dead.to_vec();
        dead.sort_unstable();
        self.pages
            .retain(|page| dead.binary_search(&(page.base_ptr() as usize)).is_err());
        self.standby.clear();
        let pages = &self.pages;
        self.open
            .retain(|offset| pages.iter().any(|page| page.cage_offset() == *offset));
        self.recount_allocated();
        before - self.pages.len()
    }

    /// Drop every free-list entry; the sweep that follows rebuilds the
    /// list from the pages that survive it.
    pub(crate) fn clear_free_list(&mut self) {
        // The linear area is a filler-capped free range the sweep finds and
        // lists again, so it must not stay reserved beside that entry.
        self.lab = None;
        self.free_list.clear();
    }

    /// Record one swept free range (already capped by a `FREE_TAG`
    /// filler header) for reuse.
    pub(crate) fn push_free_range(&mut self, offset: u32, size: usize) {
        // SAFETY: sweep supplies disjoint filler-capped dead ranges only
        // from retained pages, after clearing the previous list.
        unsafe { self.free_list.push(offset, size) };
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
    /// already proves capacity. Pages still held from an earlier reservation
    /// count toward `count`; only the shortfall comes from the cage, because
    /// returning a page zeroes it and advises the OS, and the next reservation
    /// would fault it back in.
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
        let missing = count.saturating_sub(self.standby.len());
        if missing == 0 {
            return Ok(());
        }
        let reserved = Page::new_many(SpaceKind::Old, missing).ok_or(OutOfMemory::CageExhausted)?;
        self.standby.extend(reserved);
        Ok(())
    }

    /// Empty pages currently held for promotion.
    #[must_use]
    pub fn standby_page_count(&self) -> usize {
        self.standby.len()
    }

    /// Take ownership of a page whose objects are already live.
    ///
    /// Used by the image restore path, which fills a page from a captured
    /// byte image and relocates its pointers before handing it over. The
    /// free list is untouched: the page arrives fully bump-allocated.
    pub(crate) fn adopt_page(&mut self, page: Page) {
        self.allocated += page.header().allocated_bytes;
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

    /// Bytes allocated in old-space: the sum of every page's allocated bytes.
    pub fn allocated_bytes(&self) -> usize {
        debug_assert_eq!(
            self.allocated,
            self.pages
                .iter()
                .map(|p| p.header().allocated_bytes)
                .sum::<usize>()
        );
        self.allocated
    }

    /// Recount [`Self::allocated_bytes`] from the pages a reap kept, whose
    /// allocated bytes a sweep set to what survived.
    fn recount_allocated(&mut self) {
        self.allocated = self
            .pages
            .iter()
            .map(|page| page.header().allocated_bytes)
            .sum();
    }

    /// Clear the live-bytes counter on every page so a fresh
    /// mark phase can accumulate.
    pub fn reset_live_bytes(&mut self) {
        for page in &self.pages {
            page.header_mut().live_bytes = 0;
        }
    }
}

impl Default for OldSpace {
    fn default() -> Self {
        Self::new()
    }
}

/// Large-object space: one page per oversized allocation.
/// Allocations whose total size exceeds [`LARGE_OBJECT_THRESHOLD`]
/// land in the old one, or in the young one ([`SpaceKind::NewLarge`])
/// while the nursery admits them.
pub struct LargeObjectSpace {
    kind: SpaceKind,
    pages: Vec<Page>,
}

impl LargeObjectSpace {
    /// Empty old LOS.
    pub fn new() -> Self {
        Self::of_kind(SpaceKind::Large)
    }

    /// Empty young LOS.
    pub fn new_young() -> Self {
        Self::of_kind(SpaceKind::NewLarge)
    }

    fn of_kind(kind: SpaceKind) -> Self {
        debug_assert!(matches!(kind, SpaceKind::Large | SpaceKind::NewLarge));
        Self {
            kind,
            pages: Vec::new(),
        }
    }

    /// Hand every page to `old` (whose kind it takes) when `promoted` holds
    /// for its object, and release the others; returns the released pages'
    /// allocated bytes. The young space ends empty.
    pub fn promote_or_release(
        &mut self,
        old: &mut Self,
        mut promoted: impl FnMut(&Page) -> bool,
    ) -> usize {
        debug_assert_eq!(self.kind, SpaceKind::NewLarge);
        debug_assert_eq!(old.kind, SpaceKind::Large);
        let mut released = 0;
        for page in self.pages.drain(..) {
            if promoted(&page) {
                page.set_space(SpaceKind::Large);
                old.pages.push(page);
            } else {
                released += page.header().allocated_bytes;
            }
        }
        released
    }

    /// Allocate one page for `size_aligned` bytes (caller has
    /// already aligned). Returns the cage offset of the payload
    /// (header position). The page is dedicated to this single
    /// allocation.
    pub fn alloc(&mut self, size_aligned: usize) -> Result<u32, OutOfMemory> {
        debug_assert!(size_aligned > LARGE_OBJECT_THRESHOLD);
        // The region is sized to the body, not the other way round: a
        // string or backing store larger than one page spans as many as it
        // needs, as one variable-sized chunk. A fixed one-page region would
        // make any body over ~128 KiB unallocatable, which for string
        // storage means unrepresentable.
        let span = size_aligned
            .saturating_add(PAGE_HEADER_SIZE)
            .div_ceil(PAGE_SIZE);
        let span = u32::try_from(span.max(1)).map_err(|_| OutOfMemory::AllocationTooLarge {
            requested_bytes: size_aligned as u64,
            max_bytes: u64::from(u32::MAX) * PAGE_SIZE as u64,
        })?;
        let page = Page::new_spanning(self.kind, span).ok_or(OutOfMemory::CageExhausted)?;
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

    /// Reap, as a sweep would, every old page no mark reached.
    fn reap_unmarked(old: &mut OldSpace) -> usize {
        old.begin_sweep();
        let dead: Vec<usize> = old
            .pages()
            .iter()
            .filter(|page| page.header().live_bytes == 0)
            .map(|page| page.base_ptr() as usize)
            .collect();
        old.reap_swept_pages(&dead)
    }
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
        // The split tail stays in the linear area and serves the next fit.
        let tail = old.alloc(64).expect("tail alloc");
        assert_eq!(tail, first + 64, "split tail must be reused next");
    }

    #[test]
    fn a_page_with_little_room_closes_and_its_tail_serves_the_free_list() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        ensure_cage();
        let mut old = OldSpace::new();
        // Fill the first page until less than OPEN_PAGE_MIN_ROOM remains.
        let chunk = 1024;
        let first = old.alloc(chunk).expect("first alloc");
        let fill = PAGE_PAYLOAD_SIZE / chunk - 1;
        for _ in 1..fill {
            old.alloc(chunk).expect("fill alloc");
        }
        assert_eq!(old.page_count(), 1);
        assert_eq!(old.open.len(), 1);
        // A request larger than the remaining room opens a second page and
        // closes the first; its tail joins the free list.
        let big = old.alloc(OPEN_PAGE_MIN_ROOM * 2).expect("big alloc");
        assert_eq!(old.page_count(), 2);
        assert_eq!(old.open.len(), 1, "only the new page stays open");
        assert_ne!(
            crate::page::page_base_from_offset(big),
            crate::page::page_base_from_offset(first)
        );
        // A small request reuses the closed page's tail before bumping.
        let small = old.alloc(64).expect("small alloc");
        assert_eq!(
            crate::page::page_base_from_offset(small),
            crate::page::page_base_from_offset(first),
            "the retired tail must be reused"
        );
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
        reap_unmarked(&mut old);
        assert!(old.standby.is_empty());
    }

    #[test]
    fn allocation_after_reap_cannot_follow_released_free_ranges() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        ensure_cage();
        let mut old = OldSpace::new();
        old.alloc(PAGE_PAYLOAD_SIZE - 1024)
            .expect("nearly full page");
        // The failed fit retires a free tail in the first page.
        old.alloc(2048).expect("second page");
        assert_eq!(reap_unmarked(&mut old), 2);
        old.alloc(64).expect("fresh storage after reap");
        assert_eq!(old.page_count(), 1, "allocation must own a live page");
        assert_eq!(old.allocated_bytes(), 64);
    }

    #[test]
    fn standby_pages_survive_scavenges_until_a_full_reap() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        ensure_cage();
        let mut old = OldSpace::new();
        old.alloc(64).expect("tail page");
        old.reserve_promotion_pages(3, PAGE_PAYLOAD_SIZE * 3)
            .expect("reserve");
        assert_eq!(old.standby.len(), 3);
        let free_before = crate::compressed::cage_stats()
            .expect("cage stats")
            .free_pages;
        // The next scavenge's reservation is covered by the held pages and
        // takes nothing from the cage; a larger one takes only the shortfall.
        old.reserve_promotion_pages(3, PAGE_PAYLOAD_SIZE * 3)
            .expect("reuse");
        assert_eq!(old.standby.len(), 3);
        old.reserve_promotion_pages(4, PAGE_PAYLOAD_SIZE * 4)
            .expect("shortfall");
        assert_eq!(old.standby.len(), 4);
        assert_eq!(
            crate::compressed::cage_stats()
                .expect("cage stats")
                .free_pages,
            free_before - 1
        );
        let reaped = reap_unmarked(&mut old);
        assert!(old.standby.is_empty());
        assert_eq!(
            crate::compressed::cage_stats()
                .expect("cage stats")
                .free_pages,
            free_before + 3 + reaped
        );
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
        reap_unmarked(&mut old);
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
