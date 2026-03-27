//! Heap spaces — logical partitions of the heap with different allocation
//! and collection strategies.
//!
//! - [`NewSpace`]: Young generation semi-space pair. Bump-pointer allocation
//!   in from-space; the scavenger copies survivors to to-space, then flips.
//! - [`OldSpace`]: Old generation. Mark-sweep with free-list allocation.
//!   Objects promoted from new space land here.
//! - [`LargeObjectSpace`]: Objects larger than half a page get their own
//!   individually-allocated page.

use std::ptr::NonNull;

use crate::align_up;
use crate::header::HEADER_SIZE;
use crate::page::{CELL_SIZE, PAGE_PAYLOAD_SIZE, Page, PageAllocError, SpaceKind};

// ---------------------------------------------------------------------------
// NewSpace — semi-space scavenger
// ---------------------------------------------------------------------------

/// Young generation with two semi-spaces. Only from-space is used for
/// allocation; the scavenger copies survivors to to-space and flips.
pub struct NewSpace {
    /// Pages currently accepting allocations (from-space).
    from_pages: Vec<Page>,
    /// Pages used as the copy target during scavenge (to-space).
    to_pages: Vec<Page>,
    /// Maximum total bytes across from-space pages before triggering scavenge.
    capacity: usize,
    /// Total bytes allocated in from-space since last scavenge.
    allocated_bytes: usize,
}

impl NewSpace {
    /// Creates a new space with the given capacity (bytes).
    /// Allocates one initial from-space page.
    pub fn new(capacity: usize) -> Result<Self, PageAllocError> {
        let mut space = Self {
            from_pages: Vec::new(),
            to_pages: Vec::new(),
            capacity,
            allocated_bytes: 0,
        };
        space.from_pages.push(Page::new(SpaceKind::NewFrom)?);
        Ok(space)
    }

    /// Creates a new space with default capacity (2 MB).
    pub fn with_defaults() -> Result<Self, PageAllocError> {
        Self::new(2 * 1024 * 1024)
    }

    /// Bump-allocates `size` bytes in from-space.
    ///
    /// Tries the last page first (hot path). If full, allocates a new page.
    /// Returns `None` if the new space has reached its capacity limit
    /// (caller should trigger a scavenge).
    pub fn alloc(&mut self, size: usize) -> Option<NonNull<u8>> {
        let aligned_size = align_up(size, CELL_SIZE);
        debug_assert!(aligned_size >= HEADER_SIZE, "allocation too small for header");

        // Fast path: try the current (last) page.
        if let Some(page) = self.from_pages.last()
            && let Some(ptr) = page.bump_alloc(aligned_size)
        {
            self.allocated_bytes += aligned_size;
            return Some(ptr);
        }

        // Slow path: need a new page. Check capacity.
        if self.allocated_bytes + aligned_size > self.capacity {
            return None; // Capacity exceeded → trigger scavenge
        }

        let page = Page::new(SpaceKind::NewFrom).ok()?;
        let ptr = page.bump_alloc(aligned_size);
        self.from_pages.push(page);
        if ptr.is_some() {
            self.allocated_bytes += aligned_size;
        }
        ptr
    }

    /// Whether from-space usage has exceeded the capacity threshold.
    pub fn should_scavenge(&self) -> bool {
        self.allocated_bytes >= self.capacity
    }

    /// Total bytes allocated in from-space.
    pub fn allocated_bytes(&self) -> usize {
        self.allocated_bytes
    }

    /// Number of from-space pages.
    pub fn from_page_count(&self) -> usize {
        self.from_pages.len()
    }

    /// Number of to-space pages.
    pub fn to_page_count(&self) -> usize {
        self.to_pages.len()
    }

    /// Provides mutable access to from-space pages (for scavenger root scanning).
    pub fn from_pages(&self) -> &[Page] {
        &self.from_pages
    }

    /// Provides mutable access to to-space pages (for scavenger).
    pub fn to_pages_mut(&mut self) -> &mut Vec<Page> {
        &mut self.to_pages
    }

    /// Allocates a bump region in to-space for the scavenger to copy into.
    /// Creates new to-space pages as needed.
    pub fn alloc_in_to_space(&mut self, size: usize) -> Option<NonNull<u8>> {
        let aligned_size = align_up(size, CELL_SIZE);

        // Try the current to-space page.
        if let Some(page) = self.to_pages.last()
            && let Some(ptr) = page.bump_alloc(aligned_size)
        {
            return Some(ptr);
        }

        // Allocate a new to-space page.
        let page = Page::new(SpaceKind::NewTo).ok()?;
        let ptr = page.bump_alloc(aligned_size);
        self.to_pages.push(page);
        ptr
    }

    /// Flips from-space and to-space after a scavenge completes.
    ///
    /// Old from-space pages are dropped (all remaining objects are garbage).
    /// To-space becomes the new from-space. A fresh empty to-space is ready
    /// for the next scavenge.
    pub fn flip(&mut self) {
        // Drop all from-space pages (garbage).
        self.from_pages.clear();

        // To-space becomes from-space.
        std::mem::swap(&mut self.from_pages, &mut self.to_pages);

        // Recompute allocated_bytes from surviving pages.
        self.allocated_bytes = self
            .from_pages
            .iter()
            .map(|p| p.header().allocated_bytes)
            .sum();

        // to_pages is now empty, ready for next scavenge.
        debug_assert!(self.to_pages.is_empty());
    }

    /// Resets the entire new space (drops all pages). Used during full GC
    /// when everything is promoted.
    pub fn reset(&mut self) {
        self.from_pages.clear();
        self.to_pages.clear();
        self.allocated_bytes = 0;
    }
}

// ---------------------------------------------------------------------------
// FreeBlock — inline free-list node within a page
// ---------------------------------------------------------------------------

/// A free block in old-space. Stored inline in the page's payload area.
/// The first 8 bytes overlap with where a GcHeader would be, using
/// `type_tag = FREE_BLOCK_TAG` as the discriminant so the sweeper can
/// skip free blocks during iteration.
#[repr(C)]
pub struct FreeBlock {
    /// Size of this free block in bytes (including this header).
    pub size: u32,
    /// Byte offset (from page base) of the next free block, or 0 if last.
    pub next_offset: u32,
}

/// Sentinel type tag for free blocks. Must not collide with any real type tag.
pub const FREE_BLOCK_TAG: u8 = 0xFF;

/// Minimum free block size (must fit a FreeBlock header).
pub const MIN_FREE_BLOCK_SIZE: usize = std::mem::size_of::<FreeBlock>();

const _: () = assert!(MIN_FREE_BLOCK_SIZE == 8);

// ---------------------------------------------------------------------------
// OldSpace — mark-sweep with free lists
// ---------------------------------------------------------------------------

/// Old generation space. Uses mark-sweep collection with in-page free lists.
///
/// Allocation carves regions from free blocks. When no free block is large
/// enough, a new page is allocated. After marking, the sweeper rebuilds
/// free lists from dead object gaps.
pub struct OldSpace {
    /// All old-space pages.
    pages: Vec<Page>,
    /// Total bytes allocated (live + free blocks — only live tracked accurately after sweep).
    allocated_bytes: usize,
    /// Total live bytes (updated after marking/sweeping).
    live_bytes: usize,
}

impl OldSpace {
    /// Creates an empty old space.
    pub fn new() -> Self {
        Self {
            pages: Vec::new(),
            allocated_bytes: 0,
            live_bytes: 0,
        }
    }

    /// Allocates `size` bytes in old space.
    ///
    /// Strategy (like V8):
    /// 1. Try free-list allocation in existing pages (first-fit).
    /// 2. Try bump allocation on existing pages (if they have room).
    /// 3. Allocate a new page and bump-allocate.
    pub fn alloc(&mut self, size: usize) -> Option<NonNull<u8>> {
        let aligned_size = align_up(size, CELL_SIZE);
        debug_assert!(aligned_size >= HEADER_SIZE);

        // 1. Try free-list allocation in existing pages.
        for page in &self.pages {
            if let Some(ptr) = self.alloc_from_free_list(page, aligned_size) {
                self.allocated_bytes += aligned_size;
                return Some(ptr);
            }
        }

        // 2. Try bump allocation on existing pages (last page first).
        for page in self.pages.iter().rev() {
            if let Some(ptr) = page.bump_alloc(aligned_size) {
                self.allocated_bytes += aligned_size;
                return Some(ptr);
            }
        }

        // 3. No space found — allocate a new page.
        let page = Page::new(SpaceKind::Old).ok()?;
        let ptr = page.bump_alloc(aligned_size);
        if ptr.is_some() {
            self.allocated_bytes += aligned_size;
        }
        self.pages.push(page);
        ptr
    }

    /// Provides access to all pages (for marking).
    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// Mutable access to pages (for sweeping).
    pub fn pages_mut(&mut self) -> &mut Vec<Page> {
        &mut self.pages
    }

    /// Total allocated bytes.
    pub fn allocated_bytes(&self) -> usize {
        self.allocated_bytes
    }

    /// Live bytes (accurate after sweep).
    pub fn live_bytes(&self) -> usize {
        self.live_bytes
    }

    /// Sets live bytes (called by sweeper after marking).
    pub fn set_live_bytes(&mut self, bytes: usize) {
        self.live_bytes = bytes;
    }

    /// Number of pages.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Adds a page to old space (used when promoting from new space).
    pub fn add_page(&mut self, page: Page) {
        self.pages.push(page);
    }

    /// Try to allocate from the free list of a specific page.
    fn alloc_from_free_list(&self, page: &Page, size: usize) -> Option<NonNull<u8>> {
        let header = page.header_mut();
        let mut prev_offset: Option<usize> = None;
        let mut current_offset = header.free_list_head as usize;

        while current_offset != 0 {
            let block_ptr = unsafe { page.base_ptr().add(current_offset) as *mut FreeBlock };
            let block = unsafe { &mut *block_ptr };
            let block_size = block.size as usize;

            if block_size >= size {
                let remainder = block_size - size;

                if remainder >= MIN_FREE_BLOCK_SIZE {
                    // Split: create a smaller free block after the allocation.
                    let new_block_offset = current_offset + size;
                    let new_block_ptr =
                        unsafe { page.base_ptr().add(new_block_offset) as *mut FreeBlock };
                    unsafe {
                        (*new_block_ptr).size = remainder as u32;
                        (*new_block_ptr).next_offset = block.next_offset;
                    }
                    // Update the previous link or head.
                    if let Some(prev) = prev_offset {
                        let prev_ptr = unsafe { page.base_ptr().add(prev) as *mut FreeBlock };
                        unsafe {
                            (*prev_ptr).next_offset = new_block_offset as u32;
                        }
                    } else {
                        header.free_list_head = new_block_offset as u32;
                    }
                } else {
                    // Use the entire block (no split).
                    let next = block.next_offset;
                    if let Some(prev) = prev_offset {
                        let prev_ptr = unsafe { page.base_ptr().add(prev) as *mut FreeBlock };
                        unsafe {
                            (*prev_ptr).next_offset = next;
                        }
                    } else {
                        header.free_list_head = next;
                    }
                }

                // Zero the allocated region (objects expect zero-initialized memory).
                unsafe {
                    std::ptr::write_bytes(page.base_ptr().add(current_offset), 0, size);
                }

                return NonNull::new(unsafe { page.base_ptr().add(current_offset) });
            }

            prev_offset = Some(current_offset);
            current_offset = block.next_offset as usize;
        }

        None
    }
}

impl Default for OldSpace {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// LargeObjectSpace
// ---------------------------------------------------------------------------

/// Large object space — objects exceeding half a page get their own page.
///
/// Each large object occupies one or more contiguous pages. The entire
/// page set is freed when the object is collected.
pub struct LargeObjectSpace {
    /// Each entry is (page, object_size).
    objects: Vec<(Page, usize)>,
    allocated_bytes: usize,
}

impl LargeObjectSpace {
    pub fn new() -> Self {
        Self {
            objects: Vec::new(),
            allocated_bytes: 0,
        }
    }

    /// Allocates a large object. Currently uses a single page (max ~256KB).
    /// For truly huge objects (>PAGE_PAYLOAD_SIZE), would need multi-page
    /// allocation — not implemented yet.
    pub fn alloc(&mut self, size: usize) -> Option<NonNull<u8>> {
        let aligned_size = align_up(size, CELL_SIZE);
        if aligned_size > PAGE_PAYLOAD_SIZE {
            return None; // TODO: multi-page allocation for >256KB objects
        }

        let page = Page::new(SpaceKind::Large).ok()?;
        let ptr = page.bump_alloc(aligned_size)?;
        self.allocated_bytes += aligned_size;
        self.objects.push((page, aligned_size));
        Some(ptr)
    }

    /// Provides access to all large objects (for marking).
    pub fn objects(&self) -> &[(Page, usize)] {
        &self.objects
    }

    /// Removes dead large objects after sweeping. Keeps only those at indices
    /// where `is_live[i]` is true.
    pub fn sweep(&mut self, is_live: &[bool]) {
        debug_assert_eq!(is_live.len(), self.objects.len());
        let mut new_objects = Vec::new();
        let mut new_allocated = 0usize;
        for (i, (page, size)) in self.objects.drain(..).enumerate() {
            if is_live.get(i).copied().unwrap_or(false) {
                new_allocated += size;
                new_objects.push((page, size));
            }
            // else: page is dropped → memory returned to OS
        }
        self.objects = new_objects;
        self.allocated_bytes = new_allocated;
    }

    pub fn allocated_bytes(&self) -> usize {
        self.allocated_bytes
    }

    pub fn count(&self) -> usize {
        self.objects.len()
    }
}

impl Default for LargeObjectSpace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::GcHeader;

    // -----------------------------------------------------------------------
    // NewSpace tests
    // -----------------------------------------------------------------------

    #[test]
    fn new_space_allocates_and_fills() {
        let mut space = NewSpace::new(4096).expect("new space");

        // Allocate several small objects.
        let p1 = space.alloc(16).expect("alloc 1");
        let p2 = space.alloc(32).expect("alloc 2");
        assert_ne!(p1, p2);
        assert!(space.allocated_bytes() > 0);
    }

    #[test]
    fn new_space_triggers_scavenge_at_capacity() {
        // Capacity smaller than one page's payload — after filling the first
        // page past this limit, should_scavenge returns true and alloc returns
        // None when a second page would be needed.
        let capacity = 1024;
        let mut space = NewSpace::new(capacity).expect("new space");

        // Fill up past the capacity within the first page.
        let mut total = 0;
        while total < capacity + 64 {
            // The first page has ~250KB so this always succeeds.
            space.alloc(64).expect("alloc within first page");
            total += 64;
        }

        assert!(space.should_scavenge());

        // Fill up the rest of the first page to force a second page request.
        while space.alloc(64).is_some() {
            // Eventually the page fills and alloc tries to get a new page,
            // which is denied because capacity is exceeded.
        }
        // Confirmed: alloc returned None due to capacity.
    }

    #[test]
    fn new_space_flip_clears_from_space() {
        let mut space = NewSpace::new(4096).expect("new space");

        // Allocate in from-space.
        space.alloc(64).expect("alloc");
        assert!(space.allocated_bytes() > 0);

        // Simulate scavenge: copy a survivor to to-space.
        space.alloc_in_to_space(32).expect("to-space alloc");
        assert_eq!(space.to_page_count(), 1);

        // Flip.
        space.flip();

        // Old from-space is gone. To-space is now from-space.
        assert!(space.to_pages.is_empty());
        assert!(space.from_page_count() > 0);
    }

    // -----------------------------------------------------------------------
    // OldSpace tests
    // -----------------------------------------------------------------------

    #[test]
    fn old_space_allocates() {
        let mut space = OldSpace::new();
        let ptr = space.alloc(24).expect("alloc");

        // Write a header to verify the memory is usable.
        unsafe {
            let header = ptr.as_ptr() as *mut GcHeader;
            header.write(GcHeader::new(1, 24));
            assert_eq!((*header).type_tag(), 1);
            assert_eq!((*header).size_bytes(), 24);
        }

        assert_eq!(space.page_count(), 1);
        assert!(space.allocated_bytes() >= 24);
    }

    #[test]
    fn old_space_free_list_allocation() {
        let mut space = OldSpace::new();

        // Allocate and manually create a free block in the first page.
        space.alloc(64).expect("initial alloc");
        let page = &space.pages[0];

        // Inject a free block after the first allocation.
        let free_offset = page.header().bump_cursor;
        let free_size = 128usize;
        unsafe {
            let free_ptr = page.base_ptr().add(free_offset) as *mut FreeBlock;
            (*free_ptr).size = free_size as u32;
            (*free_ptr).next_offset = 0; // Last block
        }
        page.header_mut().free_list_head = free_offset as u32;
        // Advance bump cursor past the free block so it doesn't interfere.
        page.header_mut().bump_cursor = free_offset + free_size;

        // Now allocate from the free list.
        let result = space.alloc_from_free_list(page, 48);
        assert!(result.is_some());

        // The free list should have a remainder block.
        let remaining_offset = page.header().free_list_head as usize;
        assert!(remaining_offset > 0);
        let remaining_block =
            unsafe { &*(page.base_ptr().add(remaining_offset) as *const FreeBlock) };
        assert_eq!(remaining_block.size as usize, free_size - 48);
    }

    // -----------------------------------------------------------------------
    // LargeObjectSpace tests
    // -----------------------------------------------------------------------

    #[test]
    fn large_object_space_allocates() {
        let mut space = LargeObjectSpace::new();
        let ptr = space.alloc(100_000).expect("large alloc");

        unsafe {
            let header = ptr.as_ptr() as *mut GcHeader;
            header.write(GcHeader::new(2, 100_000));
            assert_eq!((*header).size_bytes(), 100_000);
        }

        assert_eq!(space.count(), 1);
        assert!(space.allocated_bytes() >= 100_000);
    }

    #[test]
    fn large_object_space_sweep() {
        let mut space = LargeObjectSpace::new();
        space.alloc(1024).expect("obj 0");
        space.alloc(2048).expect("obj 1");
        space.alloc(4096).expect("obj 2");
        assert_eq!(space.count(), 3);

        // Keep only object 1.
        space.sweep(&[false, true, false]);
        assert_eq!(space.count(), 1);
    }
}
