//! 256 KiB page-aligned heap region; the unit of cage allocation.
//!
//! Pages are carved from the [`crate::compressed::Cage`]. Every GC
//! object lives inside a page; an object pointer's page can be
//! recovered in O(1) with the bitmask `addr & !(PAGE_SIZE - 1)`,
//! the same trick V8 / JSC use.
//!
//! # Layout
//!
//! ```text
//! ┌────────────────────────────────────────────┐  ← page base
//! │  PageHeader                                │     (cage offset == cage_offset)
//! │   - space_kind, flags, bump_cursor         │
//! │   - allocated_bytes / live_bytes           │
//! │   - survival_age (scavenger promotion)     │
//! │   - card-table bitmap (1 bit per 512 B)    │
//! │   - free-list head (old-space)             │
//! ├────────────────────────────────────────────┤  ← payload_start
//! │  Object 1 (GcHeader + payload)             │
//! │  Object 2 (GcHeader + payload)             │
//! │  …                                         │
//! └────────────────────────────────────────────┘
//! ```
//!
//! # Contents
//!
//! - [`PAGE_SIZE`] / [`CARD_SIZE`] / [`CARDS_PER_PAGE`] —
//!   layout constants.
//! - [`SpaceKind`] — which space currently owns the page.
//! - [`PageFlags`] — per-page bitfield (swept, evacuation
//!   candidate, has-pinned, …).
//! - [`PageHeader`] — the metadata block at the page base.
//! - [`Page`] — owning handle that returns the cage page on
//!   `Drop`.
//!
//! # Invariants
//!
//! - Page size is power-of-two so `page_base_of` is a single
//!   bitmask op.
//! - `bump_cursor` is byte offset from page base; objects live in
//!   `[PAGE_HEADER_SIZE, bump_cursor)`.
//! - Card-table bitmap covers exactly `PAGE_SIZE / CARD_SIZE` cards;
//!   bit `i` set means cards `[i * CARD_SIZE, (i+1) * CARD_SIZE)` are
//!   dirty.
//! - The header fits in `PAGE_HEADER_SIZE` bytes and is multiple
//!   of [`crate::OBJECT_ALIGNMENT`] so the first payload byte is
//!   aligned.
//!
//! # See also
//!
//! - [`crate::compressed`] — cage backing the page.
//! - GC architecture plan §2.3, §6 (V8 page shape).

use crate::OBJECT_ALIGNMENT;
use crate::compressed::{Cage, CagePage, cage_base};
use crate::header::GcHeader;

/// 256 KiB page size — V8 ARM64 alignment, balances metadata
/// overhead against per-page granularity.
pub const PAGE_SIZE: usize = 256 * 1024;

/// Card size: 512 B per card. Card-table bitmap covers
/// `PAGE_SIZE / CARD_SIZE` = 512 cards per page.
pub const CARD_SIZE: usize = 512;

/// Cards per page (power-of-two) — 512 entries → 64 bytes of
/// bitmap.
pub const CARDS_PER_PAGE: usize = PAGE_SIZE / CARD_SIZE;

/// Card-table bitmap byte length.
pub const CARD_BITMAP_BYTES: usize = CARDS_PER_PAGE / 8;

/// Allocation granule — every object size is rounded up to this.
pub const CELL_SIZE: usize = OBJECT_ALIGNMENT;

/// Largest payload that fits on a regular page. Anything bigger
/// goes to [`crate::space::LargeObjectSpace`].
pub const LARGE_OBJECT_THRESHOLD: usize = (PAGE_SIZE - PAGE_HEADER_SIZE) / 2;

/// Round `n` up to the next multiple of `align` (power-of-two
/// only).
#[inline]
pub const fn align_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

/// Which space currently owns the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[repr(u8)]
pub enum SpaceKind {
    /// Young generation from-space (mutator allocations land here).
    NewFrom = 0,
    /// Young generation to-space (scavenger evacuates into it).
    NewTo = 1,
    /// Old generation (survivors after promotion).
    Old = 2,
    /// Large object space (one large object per page).
    Large = 3,
}

/// Per-page flag bitfield.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageFlags(u32);

impl PageFlags {
    /// Page has been swept since the last marking cycle.
    pub const SWEPT: Self = Self(1 << 0);
    /// Page has been chosen for evacuation by a future compactor.
    pub const EVACUATION_CANDIDATE: Self = Self(1 << 1);
    /// Page contains at least one pinned object.
    pub const HAS_PINNED: Self = Self(1 << 2);

    /// Empty flag set.
    pub const fn empty() -> Self {
        Self(0)
    }
    /// Returns the raw u32 form.
    pub const fn bits(self) -> u32 {
        self.0
    }
    /// Containment test.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    /// Set bits in `other`.
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
    /// Clear bits in `other`.
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

/// Metadata block stored at the start of every page.
#[repr(C)]
pub struct PageHeader {
    /// Space owning the page.
    pub space: SpaceKind,
    /// Per-page flags.
    pub flags: PageFlags,
    /// Bump-allocation cursor (byte offset from page base).
    pub bump_cursor: usize,
    /// Total allocated bytes in this page's payload area.
    pub allocated_bytes: usize,
    /// Bytes covered by live objects after the most recent mark
    /// phase. Cleared at sweep start, accumulated during mark.
    pub live_bytes: usize,
    /// Survival counter — incremented each scavenge a young page
    /// stays in `NewFrom`. Promotion fires when this hits the
    /// threshold (see [`crate::scavenger::PROMOTE_AFTER_SURVIVALS`]).
    pub survival_age: u32,
    /// Compressed offset of this page's base inside the cage. Used
    /// by [`Page::cage_offset`] without touching the cage mutex.
    pub cage_offset: u32,
    /// Card-table bitmap — 1 bit per [`CARD_SIZE`]-byte card. Bit
    /// set ⇒ card may contain old→young pointers (generational
    /// remembered set).
    pub card_bitmap: [u8; CARD_BITMAP_BYTES],
}

impl PageHeader {
    fn init(&mut self, space: SpaceKind, cage_offset: u32) {
        self.space = space;
        self.flags = PageFlags::empty();
        self.bump_cursor = PAGE_HEADER_SIZE;
        self.allocated_bytes = 0;
        self.live_bytes = 0;
        self.survival_age = 0;
        self.cage_offset = cage_offset;
        self.card_bitmap = [0u8; CARD_BITMAP_BYTES];
    }

    /// Bytes still available for bump allocation in this page.
    pub const fn bump_remaining(&self) -> usize {
        PAGE_SIZE.saturating_sub(self.bump_cursor)
    }

    /// Mark the card containing `byte_offset` as dirty.
    #[inline]
    pub fn mark_card(&mut self, byte_offset: usize) {
        debug_assert!(byte_offset < PAGE_SIZE);
        let card = byte_offset / CARD_SIZE;
        self.card_bitmap[card / 8] |= 1u8 << (card % 8);
    }

    /// Test the dirty bit for a given byte offset.
    #[inline]
    pub fn is_card_dirty(&self, byte_offset: usize) -> bool {
        debug_assert!(byte_offset < PAGE_SIZE);
        let card = byte_offset / CARD_SIZE;
        self.card_bitmap[card / 8] & (1u8 << (card % 8)) != 0
    }

    /// Drop all card-table bits.
    #[inline]
    pub fn clear_cards(&mut self) {
        self.card_bitmap = [0u8; CARD_BITMAP_BYTES];
    }

    /// Iterate through every dirty card on this page, calling
    /// `visitor(card_index, byte_offset_start)` for each.
    pub fn for_each_dirty_card<F: FnMut(usize, usize)>(&self, mut visitor: F) {
        for (byte_idx, &byte) in self.card_bitmap.iter().enumerate() {
            if byte == 0 {
                continue;
            }
            for bit in 0..8 {
                if byte & (1 << bit) != 0 {
                    let card = byte_idx * 8 + bit;
                    visitor(card, card * CARD_SIZE);
                }
            }
        }
    }
}

/// Page header size, rounded up to the cell-alignment boundary so
/// the first payload byte is aligned.
pub const PAGE_HEADER_SIZE: usize = align_up(std::mem::size_of::<PageHeader>(), OBJECT_ALIGNMENT);
/// Bytes available for objects per page.
pub const PAGE_PAYLOAD_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

const _: () = assert!(PAGE_SIZE.is_power_of_two());
const _: () = assert!(CARD_SIZE.is_power_of_two());
const _: () = assert!(CARD_BITMAP_BYTES > 0);
const _: () = assert!(PAGE_HEADER_SIZE < PAGE_SIZE);

/// Owning handle to a cage page. Drops the page back to the cage
/// free-list when the value goes out of scope.
pub struct Page {
    /// Raw page-base pointer (within the cage).
    base: *mut u8,
    /// Cage offset of the page base.
    cage_offset: u32,
}

// SAFETY: The page is logically owned by a single GcHeap which is
// itself !Sync (see ADR-0004 / NF5). Implementing Send is required
// so a heap can be transferred between threads when an isolate is
// reassigned. Sync is *not* implemented.
unsafe impl Send for Page {}

impl Page {
    /// Carve a fresh page out of the cage and initialise its
    /// header for the given space.
    pub fn new(space: SpaceKind) -> Option<Self> {
        let CagePage { base, offset } = Cage::alloc_page()?;
        let page = Self {
            base,
            cage_offset: offset,
        };
        page.header_mut().init(space, offset);
        Some(page)
    }

    /// Returns the page-base pointer.
    #[inline]
    pub fn base_ptr(&self) -> *mut u8 {
        self.base
    }

    /// Returns the cage offset of the page base.
    #[inline]
    pub fn cage_offset(&self) -> u32 {
        self.cage_offset
    }

    /// Shared header reference.
    #[inline]
    pub fn header(&self) -> &PageHeader {
        // SAFETY: `self.base` is a valid initialised page header.
        unsafe { &*(self.base as *const PageHeader) }
    }

    /// Mutable header reference.
    ///
    /// Returns `&mut PageHeader` from `&self` — sound under the
    /// single-mutator GC model (one isolate = one thread). The
    /// borrow checker cannot enforce this at the type level
    /// because pages are reachable both from the heap's owning
    /// vec *and* from interior pointers in the worklist; we
    /// uphold uniqueness manually at the call site.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub fn header_mut(&self) -> &mut PageHeader {
        // SAFETY: see above — single-mutator invariant.
        unsafe { &mut *(self.base as *mut PageHeader) }
    }

    /// Mark a byte offset within the page as a dirty card.
    #[inline]
    pub fn mark_card(&self, byte_offset: usize) {
        self.header_mut().mark_card(byte_offset);
    }

    /// Bump-allocate `size` bytes, aligned to [`CELL_SIZE`].
    /// Returns the cage offset of the allocation, or `None` if
    /// the page is full.
    #[inline]
    pub fn bump_alloc(&self, size: usize) -> Option<u32> {
        debug_assert!(size % CELL_SIZE == 0, "size must be cell-aligned");
        let header = self.header_mut();
        let cursor = header.bump_cursor;
        let new_cursor = cursor + size;
        if new_cursor > PAGE_SIZE {
            return None;
        }
        header.bump_cursor = new_cursor;
        header.allocated_bytes += size;
        Some(self.cage_offset + cursor as u32)
    }

    /// Reset the bump cursor — called by the scavenger when the
    /// page is recycled into a fresh to-space.
    pub fn reset_bump(&self) {
        let header = self.header_mut();
        header.bump_cursor = PAGE_HEADER_SIZE;
        header.allocated_bytes = 0;
        header.live_bytes = 0;
        header.clear_cards();
    }

    /// Reassign the page's owning space.
    pub fn set_space(&self, space: SpaceKind) {
        self.header_mut().space = space;
    }

    /// Walk every allocated object in the payload area.
    ///
    /// # Safety
    ///
    /// Every header up to `bump_cursor` must be valid (always
    /// true for a page returned by [`Page::new`] which has only
    /// ever been written through [`Page::bump_alloc`]).
    pub unsafe fn for_each_object<F: FnMut(*mut GcHeader, usize)>(&self, mut visitor: F) {
        let base = self.base;
        let mut offset = PAGE_HEADER_SIZE;
        let limit = self.header().bump_cursor;
        while offset < limit {
            // SAFETY: offset is in-range; bytes were initialised by
            // the matching `bump_alloc` plus the writer of the
            // GcHeader.
            let header_ptr = unsafe { base.add(offset) as *mut GcHeader };
            let size = unsafe { (*header_ptr).size_bytes() } as usize;
            if size == 0 {
                break;
            }
            visitor(header_ptr, offset);
            offset += align_up(size, CELL_SIZE);
        }
    }

    /// Return the page-base pointer that a header pointer
    /// belongs to (O(1) bitmask).
    #[inline]
    pub fn page_base_of(ptr: *const u8) -> *mut u8 {
        let addr = ptr as usize;
        (addr & !(PAGE_SIZE - 1)) as *mut u8
    }

    /// Return a shared reference to the page header containing
    /// the given pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must point inside a live cage page.
    #[inline]
    pub unsafe fn header_of<'a>(ptr: *const u8) -> &'a PageHeader {
        let base = Self::page_base_of(ptr);
        // SAFETY: caller guarantees ptr is inside a live page.
        unsafe { &*(base as *const PageHeader) }
    }

    /// Return a mutable reference to the page header containing
    /// the given pointer.
    ///
    /// # Safety
    ///
    /// Same contract as [`Page::header_of`], plus exclusive
    /// access at the call site.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn header_of_mut<'a>(ptr: *const u8) -> &'a mut PageHeader {
        let base = Self::page_base_of(ptr);
        // SAFETY: see above.
        unsafe { &mut *(base as *mut PageHeader) }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        // SAFETY: caller of Page::new owned the page; nothing
        // else holds Gc<T> into it (GcHeap drops its pages only
        // after collection completes).
        unsafe {
            Cage::free_page(self.cage_offset);
        }
    }
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let h = self.header();
        f.debug_struct("Page")
            .field("cage_offset", &format_args!("0x{:x}", self.cage_offset))
            .field("space", &h.space)
            .field("bump_cursor", &h.bump_cursor)
            .field("allocated_bytes", &h.allocated_bytes)
            .field("survival_age", &h.survival_age)
            .finish()
    }
}

/// Recover the [`Page`] base address as a raw `*mut u8` from a
/// cage offset (without consulting the cage mutex).
#[inline]
pub fn page_base_from_offset(cage_offset: u32) -> *mut u8 {
    let base = cage_base();
    debug_assert!(!base.is_null());
    let page_base = (cage_offset as usize) & !(PAGE_SIZE - 1);
    // SAFETY: cage_offset was issued by Cage::alloc_page; the
    // page-rounded byte index is in-cage.
    unsafe { base.add(page_base) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressed::Cage;

    fn ensure_cage() {
        let _ = Cage::ensure_default();
    }

    #[test]
    fn header_size_fits() {
        const { assert!(PAGE_HEADER_SIZE < PAGE_SIZE) };
        const { assert!(PAGE_HEADER_SIZE % OBJECT_ALIGNMENT == 0) };
        const { assert!(PAGE_PAYLOAD_SIZE > 100_000) };
    }

    #[test]
    fn page_alloc_and_bump() {
        ensure_cage();
        let p = Page::new(SpaceKind::NewFrom).expect("page");
        let off1 = p.bump_alloc(64).expect("bump 1");
        let off2 = p.bump_alloc(64).expect("bump 2");
        assert!(off2 > off1);
        assert!(off1 >= p.cage_offset() + PAGE_HEADER_SIZE as u32);
    }

    #[test]
    fn page_base_of_is_o1() {
        ensure_cage();
        let p = Page::new(SpaceKind::Old).expect("page");
        let mid = unsafe { p.base_ptr().add(PAGE_SIZE / 2) };
        assert_eq!(Page::page_base_of(mid), p.base_ptr());
    }

    #[test]
    fn card_table_set_and_test() {
        ensure_cage();
        let p = Page::new(SpaceKind::Old).expect("page");
        let off = PAGE_HEADER_SIZE + 4 * CARD_SIZE;
        assert!(!p.header().is_card_dirty(off));
        p.mark_card(off);
        assert!(p.header().is_card_dirty(off));
        let mut hit = 0;
        p.header().for_each_dirty_card(|_, _| hit += 1);
        assert_eq!(hit, 1);
        p.header_mut().clear_cards();
        assert!(!p.header().is_card_dirty(off));
    }

    #[test]
    fn page_drop_returns_to_cage() {
        ensure_cage();
        let before = Cage::free_page_count();
        {
            let _p = Page::new(SpaceKind::NewFrom).expect("page");
            assert_eq!(Cage::free_page_count(), before - 1);
        }
        assert_eq!(Cage::free_page_count(), before);
    }
}
