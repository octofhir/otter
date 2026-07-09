//! Heap spaces: young (semispace), old (free-list), large object.
//!
//! # Contents
//!
//! - [`NewSpace`] — pair of semispaces with bump alloc in
//!   from-space, flipped on scavenge.
//! - [`OldSpace`] — list of pages; bump alloc inside the current
//!   active page; pages get marked-and-swept on a full GC.
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

/// Old-generation space: a list of pages. Allocation bumps in
/// the head page; full pages get marked-and-swept on a full GC.
pub struct OldSpace {
    pages: Vec<Page>,
}

impl OldSpace {
    /// Empty old-space; pages are added lazily as old-gen alloc
    /// demand arrives.
    pub fn new() -> Self {
        Self { pages: Vec::new() }
    }

    /// Bump-allocate `size_aligned` bytes in old-space, growing
    /// by one page if every existing page is full.
    pub fn alloc(&mut self, size_aligned: usize) -> Result<u32, OutOfMemory> {
        if size_aligned > PAGE_PAYLOAD_SIZE {
            return Err(OutOfMemory::AllocationTooLarge {
                requested_bytes: size_aligned as u64,
                max_bytes: PAGE_PAYLOAD_SIZE as u64,
            });
        }
        for page in self.pages.iter().rev() {
            if let Some(offset) = page.bump_alloc(size_aligned) {
                return Ok(offset);
            }
        }
        let page = Page::new(SpaceKind::Old).ok_or(OutOfMemory::CageExhausted)?;
        let offset = page
            .bump_alloc(size_aligned)
            .ok_or(OutOfMemory::AllocationTooLarge {
                requested_bytes: size_aligned as u64,
                max_bytes: PAGE_PAYLOAD_SIZE as u64,
            })?;
        self.pages.push(page);
        Ok(offset)
    }

    /// Atomically reserve empty promotion pages before a copying collection.
    ///
    /// Pages are first acquired into a temporary vector. If the cage cannot
    /// satisfy the complete request, that vector drops and the old space stays
    /// unchanged. The returned index identifies the first reserved page so
    /// unused pages can be released after the scavenge.
    pub(crate) fn reserve_promotion_pages(&mut self, count: usize) -> Result<usize, OutOfMemory> {
        let reserved = Page::new_many(SpaceKind::Old, count).ok_or(OutOfMemory::CageExhausted)?;
        let start = self.pages.len();
        self.pages.extend(reserved);
        Ok(start)
    }

    /// Return unused pages from the most recent promotion reservation.
    pub(crate) fn release_unused_promotion_pages(&mut self, start: usize) {
        let mut index = 0usize;
        self.pages.retain(|page| {
            let keep = index < start || page.header().allocated_bytes != 0;
            index += 1;
            keep
        });
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
