//! Page-aligned memory regions — the fundamental unit of heap organization.
//!
//! Every GC-managed object lives inside a [`Page`]. Pages are allocated at
//! size-aligned virtual addresses so that any object pointer can locate its
//! owning page in O(1) via bitmask: `page_base = addr & !(PAGE_SIZE - 1)`.
//!
//! # Layout
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐  ← page base
//! │  PageHeader (PAGE_HEADER_SIZE bytes)                     │     (aligned to PAGE_SIZE)
//! │    - owner_space, flags, marking_bitmap, free_list_head  │
//! │    - allocated_bytes, live_bytes                          │
//! ├──────────────────────────────────────────────────────────┤  ← payload_start
//! │  Usable area (PAGE_SIZE - PAGE_HEADER_SIZE bytes)        │
//! │    [GcHeader+Object][GcHeader+Object][  free  ][...]     │
//! └──────────────────────────────────────────────────────────┘
//! ```
//!
//! # Size
//!
//! 256 KB (262,144 bytes) on all platforms. This matches V8's ARM64 page size
//! and is a good balance between metadata overhead and granularity.
//! The usable payload area is `PAGE_SIZE - PAGE_HEADER_SIZE`.
//!
//! # Marking Bitmap
//!
//! Stored in the page header. 1 bit per [`CELL_SIZE`]-byte cell in the
//! payload area. An object's bit index is `(obj_offset - payload_offset) / CELL_SIZE`.
//! The bitmap enables O(1) mark checks without touching the object header on
//! the sweep hot path.

use std::ptr::NonNull;

use crate::header::GcHeader;
use crate::{OBJECT_ALIGNMENT, align_up};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Page size: 256 KB. Power of two for bitmask alignment computation.
pub const PAGE_SIZE: usize = 256 * 1024; // 262_144

/// Minimum allocation granule in the payload area. Objects are always a
/// multiple of CELL_SIZE bytes. Chosen to match the GcHeader alignment (8 bytes).
pub const CELL_SIZE: usize = OBJECT_ALIGNMENT; // 8

/// Maximum number of cells in the payload area.
#[cfg(test)]
const fn max_cells() -> usize {
    (PAGE_SIZE - PAGE_HEADER_SIZE) / CELL_SIZE
}

/// Number of bytes reserved for the inline marking bitmap in the page header.
/// Pre-calculated: with 256 KB pages and 8-byte cells, payload ≈ 261 KB,
/// ~32640 cells, bitmap = 4080 bytes → aligned to 4080 bytes.
/// We reserve a conservative upper bound that covers any PAGE_HEADER_SIZE.
///
/// With PAGE_HEADER_SIZE ≈ 4224 bytes (see below), payload ≈ 257920 bytes,
/// cells ≈ 32240, bitmap = 4030 bytes → fits in 4032 (8-aligned).
const MARKING_BITMAP_CAPACITY: usize = 4096; // Conservative upper bound

/// Total page header size. Must be a multiple of OBJECT_ALIGNMENT so that the
/// first object in the payload area is properly aligned.
pub const PAGE_HEADER_SIZE: usize = align_up(
    // Fixed fields
    8 // flags (u64)
    + 8 // allocated_bytes (usize on 64-bit)
    + 8 // live_bytes (usize)
    + 8 // bump_cursor (usize) — for new-space bump allocation
    + 8 // free_list_head offset (u32 + padding)
    + 8 // owner tag (u8 SpaceKind + padding)
    + MARKING_BITMAP_CAPACITY, // inline marking bitmap
    OBJECT_ALIGNMENT,
);

/// Usable payload area per page in bytes.
pub const PAGE_PAYLOAD_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

// ---------------------------------------------------------------------------
// SpaceKind
// ---------------------------------------------------------------------------

/// Which heap space owns this page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SpaceKind {
    /// Young generation from-space (allocations happen here).
    NewFrom = 0,
    /// Young generation to-space (scavenger copies survivors here).
    NewTo = 1,
    /// Old generation (mark-sweep-compact).
    Old = 2,
    /// Large object space (one object per page).
    Large = 3,
}

// ---------------------------------------------------------------------------
// Page flags
// ---------------------------------------------------------------------------

/// Per-page flag bits packed into a `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFlags(u64);

#[allow(dead_code)]
impl PageFlags {
    /// Page is an evacuation candidate (high fragmentation → will be compacted).
    pub const EVACUATION_CANDIDATE: Self = Self(1 << 0);
    /// Page has been swept since the last marking cycle.
    pub const SWEPT: Self = Self(1 << 1);
    /// Page is currently being incrementally marked.
    pub const INCREMENTALLY_MARKING: Self = Self(1 << 2);
    /// Page contains at least one pinned object (cannot be evacuated).
    pub const HAS_PINNED: Self = Self(1 << 3);

    pub const fn empty() -> Self { Self(0) }
    pub const fn bits(self) -> u64 { self.0 }
    pub const fn contains(self, other: Self) -> bool { (self.0 & other.0) == other.0 }
    pub fn insert(&mut self, other: Self) { self.0 |= other.0; }
    pub fn remove(&mut self, other: Self) { self.0 &= !other.0; }
}

// ---------------------------------------------------------------------------
// PageHeader
// ---------------------------------------------------------------------------

/// Page header stored at the base of every page.
///
/// All fields are safe to access from a single mutator thread. The marking
/// bitmap is also read by the concurrent marker but always through the
/// [`GcHeader`] atomic mark bits (the bitmap here is for the sweep phase).
#[repr(C)]
pub struct PageHeader {
    /// Which space owns this page.
    pub space: SpaceKind,
    /// Per-page flags.
    pub flags: PageFlags,
    /// Total bytes currently allocated in this page's payload area.
    pub allocated_bytes: usize,
    /// Bytes occupied by live objects (updated after marking).
    pub live_bytes: usize,
    /// Bump-pointer cursor offset from page base. Only meaningful for
    /// new-space pages. Points to the next free byte in the payload area.
    pub bump_cursor: usize,
    /// Offset of the first free block in the payload area (for old-space
    /// free-list allocation). `0` means no free blocks.
    pub free_list_head: u32,
    /// Inline marking bitmap: 1 bit per [`CELL_SIZE`]-aligned cell.
    pub marking_bitmap: [u8; MARKING_BITMAP_CAPACITY],
}

impl PageHeader {
    /// Initialize a page header for the given space.
    pub fn init(&mut self, space: SpaceKind) {
        self.space = space;
        self.flags = PageFlags::empty();
        self.allocated_bytes = 0;
        self.live_bytes = 0;
        self.bump_cursor = PAGE_HEADER_SIZE;
        self.free_list_head = 0;
        self.marking_bitmap = [0u8; MARKING_BITMAP_CAPACITY];
    }

    /// Returns the byte offset where the payload area starts.
    pub const fn payload_start() -> usize {
        PAGE_HEADER_SIZE
    }

    /// Returns the byte offset where the payload area ends (exclusive).
    pub const fn payload_end() -> usize {
        PAGE_SIZE
    }

    /// Returns how many bytes remain for bump allocation.
    pub const fn bump_remaining(&self) -> usize {
        PAGE_SIZE.saturating_sub(self.bump_cursor)
    }

    // -----------------------------------------------------------------------
    // Marking bitmap operations
    // -----------------------------------------------------------------------

    /// Sets the mark bit for the cell at the given byte offset from page base.
    pub fn set_mark_bit(&mut self, offset: usize) {
        debug_assert!(offset >= PAGE_HEADER_SIZE);
        debug_assert!(offset < PAGE_SIZE);
        let cell_index = (offset - PAGE_HEADER_SIZE) / CELL_SIZE;
        let byte_index = cell_index / 8;
        let bit_index = cell_index % 8;
        if byte_index < MARKING_BITMAP_CAPACITY {
            self.marking_bitmap[byte_index] |= 1 << bit_index;
        }
    }

    /// Clears the mark bit for the cell at the given byte offset.
    pub fn clear_mark_bit(&mut self, offset: usize) {
        debug_assert!(offset >= PAGE_HEADER_SIZE);
        let cell_index = (offset - PAGE_HEADER_SIZE) / CELL_SIZE;
        let byte_index = cell_index / 8;
        let bit_index = cell_index % 8;
        if byte_index < MARKING_BITMAP_CAPACITY {
            self.marking_bitmap[byte_index] &= !(1 << bit_index);
        }
    }

    /// Tests the mark bit for the cell at the given byte offset.
    pub fn test_mark_bit(&self, offset: usize) -> bool {
        debug_assert!(offset >= PAGE_HEADER_SIZE);
        let cell_index = (offset - PAGE_HEADER_SIZE) / CELL_SIZE;
        let byte_index = cell_index / 8;
        let bit_index = cell_index % 8;
        if byte_index < MARKING_BITMAP_CAPACITY {
            self.marking_bitmap[byte_index] & (1 << bit_index) != 0
        } else {
            false
        }
    }

    /// Clears all mark bits (O(n) but only done once per GC cycle per page).
    pub fn clear_all_mark_bits(&mut self) {
        self.marking_bitmap.fill(0);
    }
}

// ---------------------------------------------------------------------------
// Page — owned aligned allocation
// ---------------------------------------------------------------------------

/// An owned, page-aligned memory region.
///
/// Allocated via `mmap` (Unix) or `VirtualAlloc` (Windows) at a virtual
/// address aligned to [`PAGE_SIZE`]. The first bytes of the allocation are
/// the [`PageHeader`]; the rest is the payload area for GC objects.
pub struct Page {
    /// Pointer to the page-aligned base address. Never null for a live Page.
    base: NonNull<u8>,
}

// Pages are single-threaded (one isolate = one thread).
// They can be sent between threads when transferring isolate ownership.
unsafe impl Send for Page {}

impl Page {
    /// Allocates a new page-aligned region and initializes its header.
    pub fn new(space: SpaceKind) -> Result<Self, PageAllocError> {
        let base = alloc_aligned_page()?;
        let page = Self { base };
        page.header_mut().init(space);
        Ok(page)
    }

    /// Returns the page base address.
    #[inline]
    pub fn base_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// Returns a reference to the page header.
    #[inline]
    pub fn header(&self) -> &PageHeader {
        unsafe { &*(self.base.as_ptr() as *const PageHeader) }
    }

    /// Returns a mutable reference to the page header.
    #[inline]
    /// Returns a mutable reference to the page header.
    ///
    /// # Safety
    ///
    /// The caller must ensure no other references to the header exist.
    /// In practice this is enforced by the single-threaded GC model
    /// (one isolate = one thread).
    #[allow(clippy::mut_from_ref)]
    pub fn header_mut(&self) -> &mut PageHeader {
        unsafe { &mut *(self.base.as_ptr() as *mut PageHeader) }
    }

    /// Given any pointer within this page, returns the page base.
    /// This is the O(1) page lookup that V8 uses.
    #[inline]
    pub fn page_base_of(ptr: *const u8) -> *mut u8 {
        let addr = ptr as usize;
        (addr & !(PAGE_SIZE - 1)) as *mut u8
    }

    /// Given any pointer within a page, returns the page header.
    ///
    /// # Safety
    ///
    /// The pointer must point within a live, initialized [`Page`].
    #[inline]
    pub unsafe fn header_of(ptr: *const u8) -> &'static PageHeader {
        let base = Self::page_base_of(ptr);
        unsafe { &*(base as *const PageHeader) }
    }

    /// Given any pointer within a page, returns a mutable page header.
    ///
    /// # Safety
    ///
    /// The pointer must point within a live, initialized [`Page`], and the
    /// caller must ensure exclusive access.
    #[inline]
    pub unsafe fn header_of_mut(ptr: *const u8) -> &'static mut PageHeader {
        let base = Self::page_base_of(ptr);
        unsafe { &mut *(base as *mut PageHeader) }
    }

    // -----------------------------------------------------------------------
    // Bump allocation (new-space fast path)
    // -----------------------------------------------------------------------

    /// Bump-allocates `size` bytes in this page. Returns a pointer to the
    /// allocated region, or `None` if the page is full.
    ///
    /// `size` must already be aligned to [`CELL_SIZE`].
    ///
    /// This is the hot-path allocation function (~3 ns): a single pointer
    /// increment with a bounds check.
    #[inline]
    pub fn bump_alloc(&self, size: usize) -> Option<NonNull<u8>> {
        debug_assert!(size.is_multiple_of(CELL_SIZE), "size must be cell-aligned");
        let header = self.header_mut();
        let cursor = header.bump_cursor;
        let new_cursor = cursor + size;
        if new_cursor > PAGE_SIZE {
            return None;
        }
        header.bump_cursor = new_cursor;
        header.allocated_bytes += size;
        unsafe {
            let ptr = self.base.as_ptr().add(cursor);
            Some(NonNull::new_unchecked(ptr))
        }
    }

    /// Resets the bump cursor to the start of the payload area.
    /// Used by the semi-space scavenger when flipping from/to spaces.
    pub fn reset_bump(&self) {
        let header = self.header_mut();
        header.bump_cursor = PAGE_HEADER_SIZE;
        header.allocated_bytes = 0;
        header.live_bytes = 0;
    }

    /// Returns the byte offset of a pointer relative to this page's base.
    #[inline]
    pub fn offset_of(&self, ptr: *const u8) -> usize {
        let base = self.base.as_ptr() as usize;
        let addr = ptr as usize;
        debug_assert!(addr >= base && addr < base + PAGE_SIZE);
        addr - base
    }

    // -----------------------------------------------------------------------
    // Object iteration (for marking/sweeping)
    // -----------------------------------------------------------------------

    /// Iterates over all allocated objects in the payload area.
    ///
    /// Calls `visitor(header_ptr, offset)` for each object. The iteration
    /// walks forward by `header.size_bytes()` steps.
    ///
    /// # Safety
    ///
    /// The page must contain a valid sequence of [`GcHeader`]-prefixed
    /// objects from `payload_start` up to `bump_cursor` (for new-space)
    /// or through the free-list structure (for old-space).
    pub unsafe fn for_each_object<F>(&self, mut visitor: F)
    where
        F: FnMut(*mut GcHeader, usize),
    {
        let base = self.base.as_ptr();
        let mut offset = PAGE_HEADER_SIZE;
        let limit = self.header().bump_cursor;

        while offset < limit {
            let header_ptr = unsafe { base.add(offset) as *mut GcHeader };
            let size = unsafe { (*header_ptr).size_bytes() as usize };
            if size == 0 {
                break; // Zero-size sentinel or corruption
            }
            visitor(header_ptr, offset);
            offset += align_up(size, CELL_SIZE);
        }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        dealloc_aligned_page(self.base);
    }
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let h = self.header();
        f.debug_struct("Page")
            .field("base", &self.base)
            .field("space", &h.space)
            .field("allocated_bytes", &h.allocated_bytes)
            .field("bump_cursor", &h.bump_cursor)
            .field("bump_remaining", &h.bump_remaining())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Platform page allocation
// ---------------------------------------------------------------------------

/// Error returned when page allocation fails.
#[derive(Debug, Clone)]
pub struct PageAllocError;

impl std::fmt::Display for PageAllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to allocate a {PAGE_SIZE}-byte aligned page")
    }
}

impl std::error::Error for PageAllocError {}

/// Allocates a `PAGE_SIZE`-aligned region of `PAGE_SIZE` bytes.
///
/// Uses `mmap` on Unix and `VirtualAlloc` on Windows. The allocation is
/// over-sized then trimmed to guarantee alignment.
fn alloc_aligned_page() -> Result<NonNull<u8>, PageAllocError> {
    // Strategy: use std::alloc with PAGE_SIZE alignment.
    // This maps to posix_memalign on Unix (which uses mmap internally
    // for large alignments) and _aligned_malloc on Windows.
    // Simpler, portable, and the allocator handles alignment correctly.
    let layout = std::alloc::Layout::from_size_align(PAGE_SIZE, PAGE_SIZE)
        .map_err(|_| PageAllocError)?;
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    NonNull::new(ptr).ok_or(PageAllocError)
}

/// Deallocates a page previously allocated by [`alloc_aligned_page`].
fn dealloc_aligned_page(base: NonNull<u8>) {
    let layout = std::alloc::Layout::from_size_align(PAGE_SIZE, PAGE_SIZE)
        .expect("page layout must be valid");
    unsafe {
        std::alloc::dealloc(base.as_ptr(), layout);
    }
}

// ---------------------------------------------------------------------------
// Static assertions
// ---------------------------------------------------------------------------

const _: () = assert!(PAGE_SIZE.is_power_of_two(), "PAGE_SIZE must be power of two");
const _: () = assert!(
    PAGE_HEADER_SIZE < PAGE_SIZE,
    "header must leave room for payload"
);
#[allow(clippy::manual_is_multiple_of)]
const _: () = assert!(
    PAGE_HEADER_SIZE % OBJECT_ALIGNMENT == 0,
    "header must be object-aligned"
);
const _: () = assert!(
    PAGE_PAYLOAD_SIZE > 0,
    "payload area must be non-empty"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_constants_are_sane() {
        assert_eq!(PAGE_SIZE, 256 * 1024);
        const { assert!(PAGE_HEADER_SIZE < PAGE_SIZE) };
        const { assert!(PAGE_PAYLOAD_SIZE > 200_000) }; // At least ~200 KB usable
        assert!(PAGE_HEADER_SIZE.is_multiple_of(8));
        println!(
            "PAGE_HEADER_SIZE={PAGE_HEADER_SIZE}, PAGE_PAYLOAD_SIZE={PAGE_PAYLOAD_SIZE}, max_cells={}",
            max_cells()
        );
    }

    #[test]
    fn allocate_and_drop_page() {
        let page = Page::new(SpaceKind::NewFrom).expect("page alloc should succeed");
        let base = page.base_ptr();

        // Base must be aligned to PAGE_SIZE.
        assert_eq!(base as usize % PAGE_SIZE, 0);

        // Header should be initialized.
        assert_eq!(page.header().space, SpaceKind::NewFrom);
        assert_eq!(page.header().allocated_bytes, 0);
        assert_eq!(page.header().bump_cursor, PAGE_HEADER_SIZE);
    }

    #[test]
    fn page_base_of_computes_correctly() {
        let page = Page::new(SpaceKind::Old).expect("page alloc");
        let base = page.base_ptr();

        // A pointer in the middle of the page should resolve to the base.
        let mid = unsafe { base.add(PAGE_SIZE / 2) };
        assert_eq!(Page::page_base_of(mid), base);

        // The base itself should resolve to itself.
        assert_eq!(Page::page_base_of(base), base);

        // Last byte of the page.
        let last = unsafe { base.add(PAGE_SIZE - 1) };
        assert_eq!(Page::page_base_of(last), base);
    }

    #[test]
    fn bump_allocation() {
        let page = Page::new(SpaceKind::NewFrom).expect("page alloc");

        // Allocate a small object (24 bytes = header + 16 payload).
        let obj1 = page.bump_alloc(24).expect("first alloc");
        assert_eq!(page.header().allocated_bytes, 24);

        // Allocate another.
        let obj2 = page.bump_alloc(32).expect("second alloc");
        assert_eq!(page.header().allocated_bytes, 56);

        // Pointers should be within the page, after the header.
        let base = page.base_ptr() as usize;
        assert!(obj1.as_ptr() as usize >= base + PAGE_HEADER_SIZE);
        assert!(obj2.as_ptr() as usize > obj1.as_ptr() as usize);
    }

    #[test]
    fn bump_allocation_fills_page() {
        let page = Page::new(SpaceKind::NewFrom).expect("page alloc");

        // Allocate in CELL_SIZE chunks until full — this guarantees exact fit.
        let mut count = 0usize;
        while page.bump_alloc(CELL_SIZE).is_some() {
            count += 1;
        }

        assert!(count > 0);
        // Remaining space must be less than CELL_SIZE (one cell).
        assert!(page.header().bump_remaining() < CELL_SIZE);
        // No more room for even the smallest allocation.
        assert!(page.bump_alloc(CELL_SIZE).is_none());
    }

    #[test]
    fn reset_bump_clears_state() {
        let page = Page::new(SpaceKind::NewFrom).expect("page alloc");
        page.bump_alloc(1024).expect("alloc");
        assert!(page.header().allocated_bytes > 0);

        page.reset_bump();
        assert_eq!(page.header().allocated_bytes, 0);
        assert_eq!(page.header().bump_cursor, PAGE_HEADER_SIZE);
    }

    #[test]
    fn marking_bitmap_set_clear_test() {
        let page = Page::new(SpaceKind::Old).expect("page alloc");
        let offset = PAGE_HEADER_SIZE; // First cell

        assert!(!page.header().test_mark_bit(offset));

        page.header_mut().set_mark_bit(offset);
        assert!(page.header().test_mark_bit(offset));

        page.header_mut().clear_mark_bit(offset);
        assert!(!page.header().test_mark_bit(offset));
    }

    #[test]
    fn marking_bitmap_multiple_cells() {
        let page = Page::new(SpaceKind::Old).expect("page alloc");

        let cell_0 = PAGE_HEADER_SIZE;
        let cell_1 = PAGE_HEADER_SIZE + CELL_SIZE;
        let cell_100 = PAGE_HEADER_SIZE + CELL_SIZE * 100;

        page.header_mut().set_mark_bit(cell_0);
        page.header_mut().set_mark_bit(cell_100);

        assert!(page.header().test_mark_bit(cell_0));
        assert!(!page.header().test_mark_bit(cell_1));
        assert!(page.header().test_mark_bit(cell_100));

        page.header_mut().clear_all_mark_bits();
        assert!(!page.header().test_mark_bit(cell_0));
        assert!(!page.header().test_mark_bit(cell_100));
    }

    #[test]
    fn object_iteration() {
        let page = Page::new(SpaceKind::NewFrom).expect("page alloc");

        // Write three fake objects.
        let sizes = [16u32, 24, 32];
        for &size in &sizes {
            let ptr = page.bump_alloc(size as usize).expect("alloc");
            let header = unsafe { &mut *(ptr.as_ptr() as *mut GcHeader) };
            *header = GcHeader::new(1, size);
        }

        // Iterate and collect sizes.
        let mut collected_sizes = Vec::new();
        unsafe {
            page.for_each_object(|header, _offset| {
                collected_sizes.push((*header).size_bytes());
            });
        }

        assert_eq!(collected_sizes, vec![16, 24, 32]);
    }

    #[test]
    fn header_of_from_interior_pointer() {
        let page = Page::new(SpaceKind::Old).expect("page alloc");
        let ptr = page.bump_alloc(64).expect("alloc");

        // An interior pointer (e.g., field at offset 16 within the object)
        let interior = unsafe { ptr.as_ptr().add(16) };
        let recovered_header = unsafe { Page::header_of(interior) };
        assert_eq!(recovered_header.space, SpaceKind::Old);
    }
}
