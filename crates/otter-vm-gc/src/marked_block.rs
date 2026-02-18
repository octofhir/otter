//! Block-based allocation.
//!
//! Objects are allocated in fixed-size blocks (16KB). Each block is dedicated
//! to a single size class. Allocation finds a free cell via bitmap scan.
//! Sweeping is per-block — entirely-dead blocks can be reclaimed in O(1).
//!
//! ## Layout
//!
//! ```text
//! MarkedBlock (16KB total storage):
//! ┌──────────────────────────────┐
//! │ Cell 0: [u8; cell_size]      │  (GcHeader + T written by caller)
//! │ Cell 1: [u8; cell_size]      │
//! │ ...                          │
//! │ Cell K: [u8; cell_size]      │
//! └──────────────────────────────┘
//!
//! Metadata stored separately (not inline):
//!   - free_bits: bitvec, 1 = free, 0 = allocated
//!   - drop_fns / trace_fns: per-cell function pointers
//! ```

use std::cell::{Cell, RefCell};

use crate::object::{GcHeader, MarkColor};

/// Block size: 16KB.
const BLOCK_SIZE: usize = 16 * 1024;

/// Size classes for segregated allocation.
/// Objects are rounded up to the nearest size class.
/// Covers 16 bytes to 8KB. Objects > 8KB use large-object space.
const SIZE_CLASSES: &[usize] = &[
    16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 1024, 2048, 4096, 8192,
];

/// Large object threshold — objects bigger than this bypass block allocation.
pub const LARGE_OBJECT_THRESHOLD: usize = 8192;

/// Type-erased drop function for cleaning up allocations
pub type DropFn = unsafe fn(*mut u8);

/// Type-erased trace function for marking references
pub type TraceFn = unsafe fn(*const u8, &mut dyn FnMut(*const GcHeader));

/// Find the size class index for a given allocation size.
/// Returns `None` if the size exceeds the largest size class (large object).
#[inline]
pub fn size_class_index(size: usize) -> Option<usize> {
    SIZE_CLASSES.iter().position(|&sc| sc >= size)
}

/// Get the cell size for a given size class index.
#[inline]
pub fn size_class_cell_size(index: usize) -> usize {
    SIZE_CLASSES[index]
}

/// Number of size classes.
pub const NUM_SIZE_CLASSES: usize = 14; // must match SIZE_CLASSES.len()

/// A 16KB block of memory containing fixed-size cells.
///
/// Each cell can hold one GC-managed object (GcHeader + T).
/// The block tracks which cells are free via a bitvector and stores
/// per-cell drop/trace functions for polymorphic type support.
pub struct MarkedBlock {
    /// Raw storage for cells (16KB, 8-byte aligned via Vec<u64>).
    /// We use Vec<u64> to guarantee 8-byte alignment.
    storage: Vec<u64>,
    /// Cell size in bytes (one of SIZE_CLASSES).
    cell_size: usize,
    /// Number of cells that fit in this block.
    num_cells: usize,
    /// Free bitvector: bit N = 1 means cell N is free.
    /// Stored as u64 words for efficient `trailing_zeros()` scanning.
    free_bits: RefCell<Vec<u64>>,
    /// Per-cell drop function (set when cell is allocated, cleared on free).
    cell_drop_fns: RefCell<Vec<Option<DropFn>>>,
    /// Per-cell trace function (set when cell is allocated, cleared on free).
    cell_trace_fns: RefCell<Vec<Option<TraceFn>>>,
    /// Per-cell allocation size (actual size, for stats tracking).
    cell_sizes: RefCell<Vec<usize>>,
    /// Number of live (allocated) cells.
    live_count: Cell<usize>,
}

// SAFETY: MarkedBlock is only accessed from the single VM/GC thread.
// Thread confinement is enforced at the AllocationRegistry level (thread_local).
unsafe impl Send for MarkedBlock {}
unsafe impl Sync for MarkedBlock {}

impl MarkedBlock {
    /// Create a new block for the given cell size.
    pub fn new(cell_size: usize) -> Self {
        assert!(cell_size >= 16, "cell size must be at least 16 bytes");
        assert!(
            cell_size.is_multiple_of(8),
            "cell size must be 8-byte aligned"
        );

        let num_cells = BLOCK_SIZE / cell_size;
        assert!(num_cells > 0, "cell size too large for block");

        // Allocate storage as u64 array for alignment
        let storage_u64s = BLOCK_SIZE.div_ceil(8);
        let storage = vec![0u64; storage_u64s];

        // Initialize free bits: all cells start free (bit = 1)
        let num_words = num_cells.div_ceil(64);
        let mut free_bits = vec![u64::MAX; num_words];
        // Mask out bits beyond num_cells in the last word
        let remainder = num_cells % 64;
        if remainder != 0 {
            free_bits[num_words - 1] = (1u64 << remainder) - 1;
        }

        let cell_drop_fns = vec![None; num_cells];
        let cell_trace_fns = vec![None; num_cells];
        let cell_sizes = vec![0usize; num_cells];

        Self {
            storage,
            cell_size,
            num_cells,
            free_bits: RefCell::new(free_bits),
            cell_drop_fns: RefCell::new(cell_drop_fns),
            cell_trace_fns: RefCell::new(cell_trace_fns),
            cell_sizes: RefCell::new(cell_sizes),
            live_count: Cell::new(0),
        }
    }

    /// Try to allocate a cell in this block.
    ///
    /// Returns a pointer to the start of the cell (where GcHeader goes),
    /// or `None` if the block is full.
    ///
    /// # Safety
    /// The caller must initialize the cell memory (write GcHeader + value)
    /// before the next GC cycle.
    pub fn allocate(
        &self,
        actual_size: usize,
        drop_fn: DropFn,
        trace_fn: Option<TraceFn>,
    ) -> Option<*mut u8> {
        let mut free_bits = self.free_bits.borrow_mut();

        // Scan free_bits for a free cell
        for (word_idx, word) in free_bits.iter_mut().enumerate() {
            if *word == 0 {
                continue; // No free cells in this word
            }

            // Find the first set bit (first free cell)
            let bit_idx = word.trailing_zeros() as usize;
            let cell_idx = word_idx * 64 + bit_idx;

            if cell_idx >= self.num_cells {
                return None; // Past the end
            }

            // Clear the bit (mark as allocated)
            *word &= !(1u64 << bit_idx);

            // Store per-cell metadata
            {
                let mut drop_fns = self.cell_drop_fns.borrow_mut();
                drop_fns[cell_idx] = Some(drop_fn);
            }
            {
                let mut trace_fns = self.cell_trace_fns.borrow_mut();
                trace_fns[cell_idx] = trace_fn;
            }
            {
                let mut sizes = self.cell_sizes.borrow_mut();
                sizes[cell_idx] = actual_size;
            }

            self.live_count.set(self.live_count.get() + 1);

            // Calculate pointer to cell
            let offset = cell_idx * self.cell_size;
            let ptr = self.storage.as_ptr() as *mut u8;
            // SAFETY: offset is within bounds (cell_idx < num_cells, each cell_size fits)
            let cell_ptr = unsafe { ptr.add(offset) };

            return Some(cell_ptr);
        }

        None // Block is full
    }

    /// Check if this block is full (no free cells).
    #[inline]
    pub fn is_full(&self) -> bool {
        self.live_count.get() == self.num_cells
    }

    /// Check if this block is empty (all cells free).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.live_count.get() == 0
    }

    /// Get number of live cells.
    #[inline]
    pub fn live_count(&self) -> usize {
        self.live_count.get()
    }

    /// Get the cell size for this block.
    #[inline]
    pub fn cell_size(&self) -> usize {
        self.cell_size
    }

    /// Get the number of cells in this block.
    #[inline]
    pub fn num_cells(&self) -> usize {
        self.num_cells
    }

    /// Trace all live objects in this block during the mark phase.
    ///
    /// For each allocated cell that has a trace function, calls the trace
    /// function to discover references to other GC objects.
    pub fn trace_allocated(&self, tracer: &mut dyn FnMut(*const GcHeader)) {
        let base = self.storage.as_ptr() as *const u8;
        let free_bits = self.free_bits.borrow();
        let trace_fns = self.cell_trace_fns.borrow();

        for cell_idx in 0..self.num_cells {
            // Skip free cells
            let word_idx = cell_idx / 64;
            let bit_idx = cell_idx % 64;
            if free_bits[word_idx] & (1u64 << bit_idx) != 0 {
                continue;
            }

            // Cell is allocated — if it has a trace fn, look up and potentially call
            if let Some(trace_fn) = trace_fns[cell_idx] {
                let offset = cell_idx * self.cell_size;
                let header_ptr = unsafe { base.add(offset) as *const GcHeader };
                let header = unsafe { &*header_ptr };

                // Only trace gray objects (in worklist)
                if header.mark() == MarkColor::Gray {
                    // The trace_fn expects a pointer to the VALUE (after the header)
                    let value_ptr = unsafe { base.add(offset + std::mem::size_of::<GcHeader>()) };
                    unsafe {
                        trace_fn(value_ptr, tracer);
                    }
                }
            }
        }
    }

    /// Sweep this block: free all white (unreachable) cells.
    ///
    /// Returns the number of bytes reclaimed.
    pub fn sweep(&self) -> usize {
        let base = self.storage.as_ptr() as *mut u8;
        let mut free_bits = self.free_bits.borrow_mut();
        let mut drop_fns = self.cell_drop_fns.borrow_mut();
        let mut trace_fns = self.cell_trace_fns.borrow_mut();
        let mut sizes = self.cell_sizes.borrow_mut();
        let mut reclaimed: usize = 0;

        // Collect cells to drop (we'll call drop_fn after releasing borrows)
        let mut to_drop: Vec<(*mut u8, DropFn, usize)> = Vec::new();

        for cell_idx in 0..self.num_cells {
            let word_idx = cell_idx / 64;
            let bit_idx = cell_idx % 64;

            // Skip already-free cells
            if free_bits[word_idx] & (1u64 << bit_idx) != 0 {
                continue;
            }

            // Cell is allocated — check mark
            let offset = cell_idx * self.cell_size;
            let header_ptr = unsafe { base.add(offset) as *const GcHeader };
            let mark = unsafe { (*header_ptr).mark() };

            if mark == MarkColor::White {
                // Unreachable — schedule for drop
                let cell_ptr = unsafe { base.add(offset) };
                if let Some(drop_fn) = drop_fns[cell_idx] {
                    to_drop.push((cell_ptr, drop_fn, sizes[cell_idx]));
                }

                // Mark cell as free
                free_bits[word_idx] |= 1u64 << bit_idx;
                drop_fns[cell_idx] = None;
                trace_fns[cell_idx] = None;
                let size = sizes[cell_idx];
                sizes[cell_idx] = 0;
                reclaimed += size;
                self.live_count.set(self.live_count.get() - 1);
            }
        }

        // Drop borrows before calling drop functions
        // (drop_fn might trigger operations that need block access)
        drop(free_bits);
        drop(drop_fns);
        drop(trace_fns);
        drop(sizes);

        // Now call drop functions
        for (ptr, drop_fn, _) in to_drop {
            unsafe {
                drop_fn(ptr);
            }
        }

        reclaimed
    }

    /// Deallocate ALL cells without checking marks.
    /// Used during isolate teardown.
    ///
    /// Returns the number of bytes reclaimed.
    pub fn dealloc_all(&self) -> usize {
        let base = self.storage.as_ptr() as *mut u8;
        let mut free_bits = self.free_bits.borrow_mut();
        let mut drop_fns = self.cell_drop_fns.borrow_mut();
        let mut trace_fns = self.cell_trace_fns.borrow_mut();
        let mut sizes = self.cell_sizes.borrow_mut();
        let mut reclaimed: usize = 0;

        let mut to_drop: Vec<(*mut u8, DropFn)> = Vec::new();

        for cell_idx in 0..self.num_cells {
            let word_idx = cell_idx / 64;
            let bit_idx = cell_idx % 64;

            if free_bits[word_idx] & (1u64 << bit_idx) != 0 {
                continue; // Already free
            }

            let offset = cell_idx * self.cell_size;
            let cell_ptr = unsafe { base.add(offset) };
            if let Some(drop_fn) = drop_fns[cell_idx] {
                to_drop.push((cell_ptr, drop_fn));
            }

            free_bits[word_idx] |= 1u64 << bit_idx;
            drop_fns[cell_idx] = None;
            trace_fns[cell_idx] = None;
            reclaimed += sizes[cell_idx];
            sizes[cell_idx] = 0;
        }

        self.live_count.set(0);

        drop(free_bits);
        drop(drop_fns);
        drop(trace_fns);
        drop(sizes);

        for (ptr, drop_fn) in to_drop {
            unsafe {
                drop_fn(ptr);
            }
        }

        reclaimed
    }

    /// Check if a pointer belongs to this block.
    #[inline]
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.storage.as_ptr() as usize;
        let addr = ptr as usize;
        addr >= base && addr < base + BLOCK_SIZE
    }

    /// Get the header pointer for a given cell index.
    ///
    /// # Safety
    /// `cell_idx` must be < `num_cells` and the cell must be allocated.
    #[inline]
    pub unsafe fn cell_header(&self, cell_idx: usize) -> *const GcHeader {
        let base = self.storage.as_ptr() as *const u8;
        unsafe { base.add(cell_idx * self.cell_size) as *const GcHeader }
    }

    /// Iterate over all allocated cells, yielding (header_ptr, trace_fn) pairs.
    ///
    /// Used by the mark phase to find traceable objects.
    pub fn for_each_allocated<F>(&self, mut f: F)
    where
        F: FnMut(*const GcHeader, Option<TraceFn>),
    {
        let base = self.storage.as_ptr() as *const u8;
        let free_bits = self.free_bits.borrow();
        let trace_fns = self.cell_trace_fns.borrow();

        for cell_idx in 0..self.num_cells {
            let word_idx = cell_idx / 64;
            let bit_idx = cell_idx % 64;
            if free_bits[word_idx] & (1u64 << bit_idx) != 0 {
                continue; // Free
            }

            let offset = cell_idx * self.cell_size;
            let header_ptr = unsafe { base.add(offset) as *const GcHeader };
            f(header_ptr, trace_fns[cell_idx]);
        }
    }
}

/// Per-size-class directory of blocks.
///
/// Manages a list of `MarkedBlock`s all having the same cell size.
/// Allocation scans from the current block cursor forward.
pub struct BlockDirectory {
    /// The cell size for all blocks in this directory.
    cell_size: usize,
    /// All blocks in this directory.
    blocks: RefCell<Vec<MarkedBlock>>,
    /// Index of the current block to try allocating from.
    /// We start searching here and wrap around.
    cursor: Cell<usize>,
}

// SAFETY: Same thread confinement as MarkedBlock.
unsafe impl Send for BlockDirectory {}
unsafe impl Sync for BlockDirectory {}

impl BlockDirectory {
    /// Create a new directory for the given cell size.
    pub fn new(cell_size: usize) -> Self {
        Self {
            cell_size,
            blocks: RefCell::new(Vec::new()),
            cursor: Cell::new(0),
        }
    }

    /// Allocate a cell from this directory.
    ///
    /// Tries the current block, then scans forward. If all blocks are full,
    /// allocates a new block.
    ///
    /// Returns a pointer to the cell start (for GcHeader placement).
    pub fn allocate(
        &self,
        actual_size: usize,
        drop_fn: DropFn,
        trace_fn: Option<TraceFn>,
    ) -> *mut u8 {
        let mut blocks = self.blocks.borrow_mut();
        let num_blocks = blocks.len();

        if num_blocks > 0 {
            let start = self.cursor.get() % num_blocks;

            // Try from cursor forward, then wrap around
            for i in 0..num_blocks {
                let idx = (start + i) % num_blocks;
                if let Some(ptr) = blocks[idx].allocate(actual_size, drop_fn, trace_fn) {
                    // Update cursor to this block (likely has more free cells)
                    self.cursor.set(idx);
                    return ptr;
                }
            }
        }

        // All blocks full (or no blocks) — allocate a new block
        let new_block = MarkedBlock::new(self.cell_size);
        let ptr = new_block
            .allocate(actual_size, drop_fn, trace_fn)
            .expect("freshly created block should have space");
        let new_idx = blocks.len();
        blocks.push(new_block);
        self.cursor.set(new_idx);
        ptr
    }

    /// Maximum number of empty blocks to retain per `BlockDirectory` as a
    /// burst buffer. Additional empty blocks are freed to keep RSS bounded.
    const MAX_EMPTY_BLOCKS: usize = 2;

    /// Sweep all blocks, returning bytes reclaimed.
    ///
    /// After sweeping, excess empty blocks (beyond `MAX_EMPTY_BLOCKS`) are
    /// freed to prevent RSS from growing to the historical allocation peak.
    /// Two empty blocks are kept as a burst buffer for future allocations.
    pub fn sweep(&self) -> usize {
        let mut reclaimed = 0;
        {
            let blocks = self.blocks.borrow();
            for block in blocks.iter() {
                reclaimed += block.sweep();
            }
        }

        // Free excess empty blocks to bound RSS.
        let mut empty_kept = 0usize;
        self.blocks.borrow_mut().retain(|block| {
            if block.live_count() == 0 {
                if empty_kept < Self::MAX_EMPTY_BLOCKS {
                    empty_kept += 1;
                    true // keep as burst buffer
                } else {
                    false // drop block, freeing its 16 KB slab
                }
            } else {
                true // keep live blocks unconditionally
            }
        });

        // Reset cursor to 0 to search from start after sweep
        self.cursor.set(0);
        reclaimed
    }

    /// Deallocate all cells across all blocks (teardown).
    pub fn dealloc_all(&self) -> usize {
        let blocks = self.blocks.borrow();
        let mut reclaimed = 0;
        for block in blocks.iter() {
            reclaimed += block.dealloc_all();
        }
        reclaimed
    }

    /// Iterate over all allocated cells across all blocks.
    pub fn for_each_allocated<F>(&self, mut f: F)
    where
        F: FnMut(*const GcHeader, Option<TraceFn>),
    {
        let blocks = self.blocks.borrow();
        for block in blocks.iter() {
            block.for_each_allocated(&mut f);
        }
    }

    /// Get total live count across all blocks.
    pub fn live_count(&self) -> usize {
        let blocks = self.blocks.borrow();
        blocks.iter().map(|b| b.live_count()).sum()
    }

    /// Get the cell size for this directory.
    #[inline]
    pub fn cell_size(&self) -> usize {
        self.cell_size
    }

    /// Check if a pointer belongs to any block in this directory.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let blocks = self.blocks.borrow();
        blocks.iter().any(|b| b.contains(ptr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dummy drop function for testing
    unsafe fn dummy_drop(_ptr: *mut u8) {}

    /// Dummy trace function for testing
    unsafe fn dummy_trace(_ptr: *const u8, _tracer: &mut dyn FnMut(*const GcHeader)) {}

    #[test]
    fn test_size_class_index() {
        assert_eq!(size_class_index(1), Some(0)); // → 16
        assert_eq!(size_class_index(16), Some(0)); // → 16
        assert_eq!(size_class_index(17), Some(1)); // → 32
        assert_eq!(size_class_index(32), Some(1)); // → 32
        assert_eq!(size_class_index(33), Some(2)); // → 48
        assert_eq!(size_class_index(8192), Some(13)); // → 8192
        assert_eq!(size_class_index(8193), None); // Large object
    }

    #[test]
    fn test_marked_block_creation() {
        let block = MarkedBlock::new(64);
        assert_eq!(block.cell_size(), 64);
        assert_eq!(block.num_cells(), BLOCK_SIZE / 64); // 256
        assert!(block.is_empty());
        assert!(!block.is_full());
    }

    #[test]
    fn test_marked_block_allocate() {
        let block = MarkedBlock::new(64);
        let ptr1 = block.allocate(64, dummy_drop, Some(dummy_trace));
        assert!(ptr1.is_some());
        assert_eq!(block.live_count(), 1);

        let ptr2 = block.allocate(64, dummy_drop, None);
        assert!(ptr2.is_some());
        assert_eq!(block.live_count(), 2);

        // Pointers should be different and cell_size apart
        let p1 = ptr1.unwrap() as usize;
        let p2 = ptr2.unwrap() as usize;
        assert_eq!(p2 - p1, 64);
    }

    #[test]
    fn test_marked_block_fill() {
        let block = MarkedBlock::new(BLOCK_SIZE); // 1 cell per block
        assert_eq!(block.num_cells(), 1);

        let ptr = block.allocate(BLOCK_SIZE, dummy_drop, None);
        assert!(ptr.is_some());
        assert!(block.is_full());

        let ptr2 = block.allocate(BLOCK_SIZE, dummy_drop, None);
        assert!(ptr2.is_none());
    }

    #[test]
    fn test_marked_block_sweep() {
        let block = MarkedBlock::new(64);

        // Allocate two cells
        let ptr1 = block.allocate(48, dummy_drop, None).unwrap();
        let ptr2 = block.allocate(48, dummy_drop, None).unwrap();
        assert_eq!(block.live_count(), 2);

        // Mark ptr1's header as Black (reachable), leave ptr2 as White
        unsafe {
            let header1 = &*(ptr1 as *const GcHeader);
            header1.set_mark(MarkColor::Black);
            // ptr2's header stays White (default)
        }

        // Sweep — should reclaim ptr2
        let reclaimed = block.sweep();
        assert!(reclaimed > 0);
        assert_eq!(block.live_count(), 1);

        // ptr1 should still be allocated, ptr2's slot should be free
        // Allocate again — should reuse ptr2's slot
        let ptr3 = block.allocate(48, dummy_drop, None).unwrap();
        assert_eq!(ptr3, ptr2); // Same slot reused
    }

    #[test]
    fn test_marked_block_dealloc_all() {
        let block = MarkedBlock::new(64);
        for _ in 0..10 {
            block.allocate(48, dummy_drop, None);
        }
        assert_eq!(block.live_count(), 10);

        let reclaimed = block.dealloc_all();
        assert!(reclaimed > 0);
        assert!(block.is_empty());
    }

    #[test]
    fn test_block_directory_allocate() {
        let dir = BlockDirectory::new(64);

        let ptr1 = dir.allocate(48, dummy_drop, None);
        assert!(!ptr1.is_null());

        let ptr2 = dir.allocate(48, dummy_drop, Some(dummy_trace));
        assert!(!ptr2.is_null());
        assert_ne!(ptr1, ptr2);

        assert_eq!(dir.live_count(), 2);
    }

    #[test]
    fn test_block_directory_new_block_on_full() {
        // Use a large cell size so block fills quickly
        let dir = BlockDirectory::new(BLOCK_SIZE); // 1 cell per block

        let ptr1 = dir.allocate(BLOCK_SIZE, dummy_drop, None);
        assert!(!ptr1.is_null());
        // This should trigger a new block
        let ptr2 = dir.allocate(BLOCK_SIZE, dummy_drop, None);
        assert!(!ptr2.is_null());
        assert_ne!(ptr1, ptr2);

        assert_eq!(dir.live_count(), 2);
    }

    #[test]
    fn test_block_directory_sweep_and_reuse() {
        let dir = BlockDirectory::new(64);

        // Allocate a cell
        let ptr = dir.allocate(48, dummy_drop, None);

        // Leave it White (unreachable)
        // Sweep
        let reclaimed = dir.sweep();
        assert!(reclaimed > 0);
        assert_eq!(dir.live_count(), 0);

        // Allocate again — should reuse the freed slot
        let ptr2 = dir.allocate(48, dummy_drop, None);
        assert_eq!(ptr, ptr2);
    }

    #[test]
    fn test_block_contains() {
        let block = MarkedBlock::new(64);
        let ptr = block.allocate(48, dummy_drop, None).unwrap();

        assert!(block.contains(ptr));

        // Some random pointer should not be contained
        let random: *const u8 = 0x12345678 as *const u8;
        assert!(!block.contains(random));
    }

    #[test]
    fn test_for_each_allocated() {
        let block = MarkedBlock::new(64);
        block.allocate(48, dummy_drop, Some(dummy_trace));
        block.allocate(48, dummy_drop, None);
        block.allocate(48, dummy_drop, Some(dummy_trace));

        let mut count = 0;
        let mut with_trace = 0;
        block.for_each_allocated(|_header, trace_fn| {
            count += 1;
            if trace_fn.is_some() {
                with_trace += 1;
            }
        });

        assert_eq!(count, 3);
        assert_eq!(with_trace, 2);
    }
}
