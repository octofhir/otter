//! Nursery (young generation) bump allocator for generational GC.
//!
//! Short-lived objects are bump-allocated in a contiguous nursery region.
//! Minor GCs only scan the nursery + remembered set, reclaiming most
//! transient objects without scanning the entire heap.
//!
//! Uses sticky mark-bit (non-moving) collection:
//! - Live objects stay in place, their mark bits are "stuck" on
//! - Dead objects' memory is reclaimed via a free list
//! - No pointer updates needed (safe for Rust's ownership model)
//!
//! Reference: <https://v8.dev/blog/minor-mark-sweep>

use std::alloc::Layout;
use std::cell::Cell;

use crate::marked_block::DropFn;
use crate::object::GcHeader;

/// Default nursery size: 2MB.
///
/// Chosen to fit in L2 cache on most modern CPUs. Smaller than V8's default
/// (4MB) because OtterJS is embedding-first and we want bounded memory.
const DEFAULT_NURSERY_SIZE: usize = 2 * 1024 * 1024;

/// Minimum alignment for nursery allocations.
///
/// Must be 16 to support types with align(16) requirements (e.g. temporal_rs
/// types which contain i128 fields). The nursery region itself is allocated
/// with this alignment, and every bump-pointer advance is rounded up to it.
const MIN_ALIGN: usize = 16;

/// Maximum number of minor GC survivals before an object is considered tenured.
///
/// After this many survivals, the object's `is_young` flag is cleared and it
/// becomes part of the old generation. With sticky mark-bit, this just means
/// we stop scanning it during minor GC.
const TENURE_THRESHOLD: u8 = 2;

/// Per-cell metadata for nursery allocations.
///
/// Stored in a parallel array (not inline with cells) to keep the nursery
/// region contiguous for bump allocation.
struct NurseryCell {
    /// Offset from nursery base where this cell starts.
    offset: usize,
    /// Actual allocation size (GcHeader + T).
    size: usize,
    /// Drop function for this cell.
    drop_fn: DropFn,
    /// Number of minor GC cycles this object has survived.
    survival_count: u8,
    /// Whether this cell is still live (not yet freed).
    live: bool,
}

/// Nursery bump allocator for the young generation.
///
/// Allocates objects sequentially in a contiguous memory region.
/// When the nursery fills up, a minor GC is triggered.
pub struct Nursery {
    /// Base pointer of the nursery region.
    base: *mut u8,
    /// Layout used to allocate the nursery region (for dealloc).
    layout: Layout,
    /// Total size of the nursery region in bytes.
    size: usize,
    /// Current bump pointer offset from base.
    cursor: Cell<usize>,
    /// Metadata for each allocated cell.
    cells: std::cell::RefCell<Vec<NurseryCell>>,
    /// Number of live cells.
    live_count: Cell<usize>,
    /// Total bytes in live cells (for stats).
    live_bytes: Cell<usize>,
}

// SAFETY: Nursery is only accessed from the single VM/GC thread.
// Thread confinement is enforced by the Isolate abstraction.
unsafe impl Send for Nursery {}
unsafe impl Sync for Nursery {}

impl Nursery {
    /// Create a new nursery with the default size (2MB).
    pub fn new() -> Self {
        Self::with_size(DEFAULT_NURSERY_SIZE)
    }

    /// Create a new nursery with a custom size.
    pub fn with_size(size: usize) -> Self {
        let layout =
            Layout::from_size_align(size, MIN_ALIGN).expect("nursery layout should be valid");
        // SAFETY: layout is valid and non-zero.
        let base = unsafe { std::alloc::alloc_zeroed(layout) };
        if base.is_null() {
            std::alloc::handle_alloc_error(layout);
        }

        Self {
            base,
            layout,
            size,
            cursor: Cell::new(0),
            cells: std::cell::RefCell::new(Vec::with_capacity(1024)),
            live_count: Cell::new(0),
            live_bytes: Cell::new(0),
        }
    }

    /// Try to bump-allocate `alloc_size` bytes in the nursery.
    ///
    /// Returns a pointer to the start of the cell (where GcHeader goes),
    /// or `None` if the nursery doesn't have enough space.
    ///
    /// This is the fast path (~3ns): just increment the bump pointer.
    #[inline]
    pub fn alloc(&self, alloc_size: usize, drop_fn: DropFn) -> Option<*mut u8> {
        let cursor = self.cursor.get();
        let aligned = align_up(cursor, MIN_ALIGN);
        let end = aligned + alloc_size;

        if end > self.size {
            return None; // Nursery full
        }

        self.cursor.set(end);

        // Record cell metadata
        self.cells.borrow_mut().push(NurseryCell {
            offset: aligned,
            size: alloc_size,
            drop_fn,
            survival_count: 0,
            live: true,
        });
        self.live_count.set(self.live_count.get() + 1);
        self.live_bytes.set(self.live_bytes.get() + alloc_size);

        // SAFETY: aligned is within bounds (we checked end <= size).
        let ptr = unsafe { self.base.add(aligned) };
        Some(ptr)
    }

    /// Check if a pointer belongs to this nursery.
    #[inline]
    pub fn contains(&self, ptr: *const u8) -> bool {
        let addr = ptr as usize;
        let base = self.base as usize;
        addr >= base && addr < base + self.size
    }

    /// Get the nursery base address (for pointer arithmetic).
    #[inline]
    pub fn base_addr(&self) -> usize {
        self.base as usize
    }

    /// Current number of bytes used by the bump pointer.
    #[inline]
    pub fn cursor(&self) -> usize {
        self.cursor.get()
    }

    /// Total nursery capacity in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.size
    }

    /// Number of live cells.
    #[inline]
    pub fn live_count(&self) -> usize {
        self.live_count.get()
    }

    /// Bytes occupied by live cells.
    #[inline]
    pub fn live_bytes(&self) -> usize {
        self.live_bytes.get()
    }

    /// Percentage of nursery used (0.0 to 1.0).
    #[inline]
    pub fn usage(&self) -> f64 {
        self.cursor.get() as f64 / self.size as f64
    }

    /// Sweep the nursery after a minor GC mark phase.
    ///
    /// Objects that are still White (unreachable) are freed.
    /// Objects that are Black (reachable) survive:
    /// - If they've survived `TENURE_THRESHOLD` minor GCs, they are tenured
    ///   (their `is_young` flag is cleared on the GcHeader).
    /// - Otherwise their survival count is incremented.
    ///
    /// Returns (bytes_reclaimed, objects_tenured).
    pub fn sweep_after_minor_gc(&self) -> (usize, usize) {
        use crate::object::MarkColor;

        let mut reclaimed: usize = 0;
        let mut tenured: usize = 0;
        let mut cells_to_drop: Vec<(*mut u8, DropFn)> = Vec::new();

        {
            let mut cells = self.cells.borrow_mut();
            for cell in cells.iter_mut() {
                if !cell.live {
                    continue;
                }

                let header_ptr = unsafe { self.base.add(cell.offset) as *const GcHeader };
                let mark = unsafe { (*header_ptr).mark() };

                if mark == MarkColor::White {
                    // Dead — schedule for drop
                    let cell_ptr = unsafe { self.base.add(cell.offset) };
                    cells_to_drop.push((cell_ptr, cell.drop_fn));
                    cell.live = false;
                    reclaimed += cell.size;
                    self.live_count.set(self.live_count.get() - 1);
                    self.live_bytes.set(self.live_bytes.get() - cell.size);
                } else {
                    // Survived — increment survival count
                    cell.survival_count += 1;

                    if cell.survival_count >= TENURE_THRESHOLD {
                        // Tenure: clear is_young flag on header
                        unsafe {
                            (*header_ptr).set_tenured();
                        }
                        tenured += 1;
                    }
                }
            }
        }

        // Call drop functions after releasing the cells borrow
        for (ptr, drop_fn) in cells_to_drop {
            unsafe {
                drop_fn(ptr);
            }
        }

        (reclaimed, tenured)
    }

    /// Sweep the nursery after a young-only minor GC.
    ///
    /// Like `sweep_after_minor_gc` but skips tenured cells. In young-only
    /// mode, tenured objects aren't marked (they're treated as old-gen) so
    /// they'd appear White — we must NOT free them.
    ///
    /// Returns (bytes_reclaimed, objects_tenured).
    pub fn sweep_young_only(&self) -> (usize, usize) {
        use crate::object::MarkColor;

        let mut reclaimed: usize = 0;
        let mut tenured: usize = 0;
        let mut cells_to_drop: Vec<(*mut u8, DropFn)> = Vec::new();

        {
            let mut cells = self.cells.borrow_mut();
            for cell in cells.iter_mut() {
                if !cell.live {
                    continue;
                }

                let header_ptr = unsafe { self.base.add(cell.offset) as *const GcHeader };

                // Skip tenured cells — they weren't marked in young-only mode.
                if unsafe { !(*header_ptr).is_young() } {
                    continue;
                }

                let mark = unsafe { (*header_ptr).mark() };

                if mark == MarkColor::White {
                    // Dead young object — schedule for drop
                    let cell_ptr = unsafe { self.base.add(cell.offset) };
                    cells_to_drop.push((cell_ptr, cell.drop_fn));
                    cell.live = false;
                    reclaimed += cell.size;
                    self.live_count.set(self.live_count.get() - 1);
                    self.live_bytes.set(self.live_bytes.get() - cell.size);
                } else {
                    // Survived — increment survival count
                    cell.survival_count += 1;

                    if cell.survival_count >= TENURE_THRESHOLD {
                        // Tenure: clear is_young flag on header
                        unsafe {
                            (*header_ptr).set_tenured();
                        }
                        tenured += 1;
                    }
                }
            }
        }

        // Call drop functions after releasing the cells borrow
        for (ptr, drop_fn) in cells_to_drop {
            unsafe {
                drop_fn(ptr);
            }
        }

        (reclaimed, tenured)
    }

    /// Compact the nursery by resetting the bump pointer.
    ///
    /// Called after minor GC when all dead objects have been swept.
    /// If there are still live objects, we rebuild the cells list and
    /// cannot reset the bump pointer (sticky mark-bit = non-moving).
    ///
    /// If all objects are dead or tenured, we can fully reset.
    pub fn compact_if_possible(&self) {
        let mut cells = self.cells.borrow_mut();

        // Remove dead cells from the metadata
        cells.retain(|c| c.live);

        // If no live young objects remain, reset the bump pointer
        if cells.is_empty() {
            self.cursor.set(0);
        }
        // Otherwise: bump pointer stays (non-moving GC).
        // Future allocations bump from the current cursor.
        // The dead slots between live objects are wasted until
        // a major GC or until all nursery objects are tenured/dead.
    }

    /// Iterate over all live cells, yielding header pointers.
    ///
    /// Used by the minor GC mark phase to find nursery objects.
    pub fn for_each_live<F>(&self, mut f: F)
    where
        F: FnMut(*const GcHeader),
    {
        let cells = self.cells.borrow();
        for cell in cells.iter() {
            if cell.live {
                let header_ptr = unsafe { self.base.add(cell.offset) as *const GcHeader };
                f(header_ptr);
            }
        }
    }

    /// Deallocate all nursery cells (teardown).
    ///
    /// Called during runtime shutdown.
    pub fn dealloc_all(&self) -> usize {
        let mut reclaimed: usize = 0;
        let mut cells_to_drop: Vec<(*mut u8, DropFn)> = Vec::new();

        {
            let mut cells = self.cells.borrow_mut();
            for cell in cells.iter_mut() {
                if cell.live {
                    let cell_ptr = unsafe { self.base.add(cell.offset) };
                    cells_to_drop.push((cell_ptr, cell.drop_fn));
                    reclaimed += cell.size;
                    cell.live = false;
                }
            }
            cells.clear();
        }

        for (ptr, drop_fn) in cells_to_drop {
            unsafe {
                drop_fn(ptr);
            }
        }

        self.live_count.set(0);
        self.live_bytes.set(0);
        self.cursor.set(0);

        reclaimed
    }
}

impl Default for Nursery {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Nursery {
    fn drop(&mut self) {
        // Dealloc all live cells first
        self.dealloc_all();

        // Free the nursery region
        // SAFETY: base was allocated with this layout in new().
        unsafe {
            std::alloc::dealloc(self.base, self.layout);
        }
    }
}

/// Align `offset` up to the given alignment.
#[inline]
const fn align_up(offset: usize, align: usize) -> usize {
    (offset + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{GcHeader, MarkColor, tags};

    /// Dummy drop function for testing
    unsafe fn dummy_drop(_ptr: *mut u8) {}

    #[test]
    fn test_nursery_alloc_basic() {
        let nursery = Nursery::with_size(4096);

        let ptr1 = nursery.alloc(64, dummy_drop);
        assert!(ptr1.is_some());
        assert_eq!(nursery.live_count(), 1);

        let ptr2 = nursery.alloc(64, dummy_drop);
        assert!(ptr2.is_some());
        assert_eq!(nursery.live_count(), 2);

        // Pointers should be different
        assert_ne!(ptr1.unwrap(), ptr2.unwrap());
    }

    #[test]
    fn test_nursery_alloc_full() {
        let nursery = Nursery::with_size(128);

        // Fill the nursery
        let ptr1 = nursery.alloc(64, dummy_drop);
        assert!(ptr1.is_some());

        let ptr2 = nursery.alloc(64, dummy_drop);
        assert!(ptr2.is_some());

        // Should fail — no space left
        let ptr3 = nursery.alloc(64, dummy_drop);
        assert!(ptr3.is_none());
    }

    #[test]
    fn test_nursery_contains() {
        let nursery = Nursery::with_size(4096);

        let ptr = nursery.alloc(64, dummy_drop).unwrap();
        assert!(nursery.contains(ptr));

        // Random pointer should not be contained
        let random: *const u8 = 0x12345678 as *const u8;
        assert!(!nursery.contains(random));
    }

    #[test]
    fn test_nursery_sweep() {
        let nursery = Nursery::with_size(4096);

        // Allocate two cells and write GcHeaders
        let ptr1 = nursery.alloc(64, dummy_drop).unwrap();
        let ptr2 = nursery.alloc(64, dummy_drop).unwrap();

        // Initialize headers
        unsafe {
            std::ptr::write(ptr1 as *mut GcHeader, GcHeader::new(tags::OBJECT));
            std::ptr::write(ptr2 as *mut GcHeader, GcHeader::new(tags::OBJECT));
        }

        // Mark ptr1 as reachable, leave ptr2 as white
        unsafe {
            (*(ptr1 as *const GcHeader)).set_mark(MarkColor::Black);
        }

        let (reclaimed, tenured) = nursery.sweep_after_minor_gc();
        assert_eq!(reclaimed, 64); // ptr2 was reclaimed
        assert_eq!(tenured, 0);
        assert_eq!(nursery.live_count(), 1);
    }

    #[test]
    fn test_nursery_tenure() {
        let nursery = Nursery::with_size(4096);

        let ptr = nursery.alloc(64, dummy_drop).unwrap();

        // Initialize header with is_young flag
        unsafe {
            std::ptr::write(ptr as *mut GcHeader, GcHeader::new_young(tags::OBJECT));
        }

        // Survive TENURE_THRESHOLD minor GCs
        for _ in 0..TENURE_THRESHOLD {
            // Mark as reachable
            unsafe {
                (*(ptr as *const GcHeader)).set_mark(MarkColor::Black);
            }
            let (_, tenured) = nursery.sweep_after_minor_gc();
            if tenured > 0 {
                // Object was tenured
                unsafe {
                    assert!(!(*(ptr as *const GcHeader)).is_young());
                }
                return;
            }
            // Reset mark for next cycle
            crate::object::bump_mark_version();
        }
    }

    #[test]
    fn test_nursery_compact_after_all_dead() {
        let nursery = Nursery::with_size(4096);

        let ptr = nursery.alloc(64, dummy_drop).unwrap();
        unsafe {
            std::ptr::write(ptr as *mut GcHeader, GcHeader::new(tags::OBJECT));
        }
        // Leave as white (dead)

        nursery.sweep_after_minor_gc();
        nursery.compact_if_possible();

        // Bump pointer should be reset
        assert_eq!(nursery.cursor(), 0);
        assert_eq!(nursery.live_count(), 0);
    }

    #[test]
    fn test_nursery_dealloc_all() {
        let nursery = Nursery::with_size(4096);

        for _ in 0..10 {
            nursery.alloc(64, dummy_drop);
        }
        assert_eq!(nursery.live_count(), 10);

        let reclaimed = nursery.dealloc_all();
        assert!(reclaimed > 0);
        assert_eq!(nursery.live_count(), 0);
        assert_eq!(nursery.cursor(), 0);
    }
}
