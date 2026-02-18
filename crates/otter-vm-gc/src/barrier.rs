//! Write barrier implementation for concurrent/incremental GC
//!
//! Write barriers are essential for maintaining GC invariants during mutation.
//! We implement:
//! - Insertion barrier (Dijkstra-style): when writing a reference into an object
//! - Deletion barrier (Yuasa-style): when removing/overwriting a reference
//! - Card marking for generational GC

use crate::object::{GcHeader, MarkColor};
use std::cell::RefCell;
use std::collections::HashSet;

/// Size of a card in bytes (typically 512 bytes)
pub const CARD_SIZE: usize = 512;

/// Card state
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    /// Card has no cross-generation pointers
    Clean = 0,
    /// Card may contain cross-generation pointers
    Dirty = 1,
}

/// Card table for tracking dirty cards
///
/// Divides the heap into fixed-size cards. When an old object
/// stores a reference to a young object, the card is marked dirty.
pub struct CardTable {
    /// Base address of the heap region
    base: usize,
    /// Size of the heap region
    size: usize,
    /// Card bytes (one byte per card)
    cards: Vec<u8>,
}

impl CardTable {
    /// Create a new card table for a heap region
    pub fn new(base: usize, size: usize) -> Self {
        let num_cards = size.div_ceil(CARD_SIZE);
        Self {
            base,
            size,
            cards: vec![CardState::Clean as u8; num_cards],
        }
    }

    /// Mark the card containing an address as dirty
    pub fn mark_card(&self, addr: usize) {
        if addr >= self.base && addr < self.base + self.size {
            let card_index = (addr - self.base) / CARD_SIZE;
            if card_index < self.cards.len() {
                // Use volatile store for visibility
                // SAFETY: We're writing a single byte within bounds
                unsafe {
                    let ptr = self.cards.as_ptr().add(card_index) as *mut u8;
                    std::ptr::write_volatile(ptr, CardState::Dirty as u8);
                }
            }
        }
    }

    /// Check if a card is dirty
    pub fn is_dirty(&self, addr: usize) -> bool {
        if addr >= self.base && addr < self.base + self.size {
            let card_index = (addr - self.base) / CARD_SIZE;
            if card_index < self.cards.len() {
                return self.cards[card_index] == CardState::Dirty as u8;
            }
        }
        false
    }

    /// Clear all cards (after GC)
    pub fn clear(&mut self) {
        self.cards.fill(CardState::Clean as u8);
    }

    /// Iterate over dirty cards, returning their address ranges
    pub fn dirty_cards(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.cards
            .iter()
            .enumerate()
            .filter(|(_, state)| **state == CardState::Dirty as u8)
            .map(|(idx, _)| {
                let start = self.base + idx * CARD_SIZE;
                let end = (start + CARD_SIZE).min(self.base + self.size);
                (start, end)
            })
    }

    /// Number of dirty cards
    pub fn dirty_count(&self) -> usize {
        self.cards
            .iter()
            .filter(|state| **state == CardState::Dirty as u8)
            .count()
    }
}

/// Write barrier buffer for batching barrier operations
///
/// Instead of processing each write barrier immediately, we buffer
/// them and process during GC.
pub struct WriteBarrierBuffer {
    /// Buffered objects that need to be re-scanned
    entries: RefCell<Vec<*const GcHeader>>,
    /// Maximum buffer size before flush
    max_size: usize,
}

impl WriteBarrierBuffer {
    /// Create a new buffer with default capacity
    pub fn new() -> Self {
        Self::with_capacity(1024)
    }

    /// Create a new buffer with specific capacity
    pub fn with_capacity(max_size: usize) -> Self {
        Self {
            entries: RefCell::new(Vec::with_capacity(max_size)),
            max_size,
        }
    }

    /// Add an entry to the buffer
    ///
    /// Returns true if buffer is full and should be flushed
    pub fn push(&self, ptr: *const GcHeader) -> bool {
        let mut entries = self.entries.borrow_mut();
        entries.push(ptr);
        entries.len() >= self.max_size
    }

    /// Take all entries from the buffer
    pub fn drain(&self) -> Vec<*const GcHeader> {
        let mut entries = self.entries.borrow_mut();
        std::mem::take(&mut *entries)
    }

    /// Check if buffer is empty
    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }

    /// Get number of entries
    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }
}

impl Default for WriteBarrierBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: WriteBarrierBuffer is only accessed from the single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
unsafe impl Send for WriteBarrierBuffer {}
unsafe impl Sync for WriteBarrierBuffer {}

/// Remembered set for tracking cross-generation references
///
/// Records old-to-young pointers to serve as additional roots
/// during young generation collection.
pub struct RememberedSet {
    /// Set of old-gen objects pointing to young-gen objects
    entries: RefCell<HashSet<*const GcHeader>>,
}

impl RememberedSet {
    /// Create a new remembered set
    pub fn new() -> Self {
        Self {
            entries: RefCell::new(HashSet::new()),
        }
    }

    /// Add an entry to the remembered set
    pub fn add(&self, ptr: *const GcHeader) {
        self.entries.borrow_mut().insert(ptr);
    }

    /// Remove an entry
    pub fn remove(&self, ptr: *const GcHeader) {
        self.entries.borrow_mut().remove(&ptr);
    }

    /// Check if contains entry
    pub fn contains(&self, ptr: *const GcHeader) -> bool {
        self.entries.borrow().contains(&ptr)
    }

    /// Get all entries as roots
    pub fn roots(&self) -> Vec<*const GcHeader> {
        self.entries.borrow().iter().copied().collect()
    }

    /// Clear the set
    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
    }

    /// Number of entries
    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }
}

impl Default for RememberedSet {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: RememberedSet is only accessed from the single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
unsafe impl Send for RememberedSet {}
unsafe impl Sync for RememberedSet {}

/// Insertion barrier (Dijkstra-style)
///
/// Called when storing a reference `to` into object `from`.
/// Ensures that if `from` is black and `to` is white, `to` becomes gray.
///
/// # Safety
/// Both pointers must be valid and point to live GcHeader objects.
#[inline]
pub unsafe fn insertion_barrier(from: *const GcHeader, to: *const GcHeader) {
    if from.is_null() || to.is_null() {
        return;
    }

    // SAFETY: Caller guarantees pointers are valid
    let from_header = unsafe { &*from };
    let to_header = unsafe { &*to };

    // Dijkstra barrier: if storing white into black, gray the target
    if from_header.mark() == MarkColor::Black && to_header.mark() == MarkColor::White {
        to_header.set_mark(MarkColor::Gray);
    }
}

/// Insertion barrier with write barrier buffer
///
/// Like insertion_barrier, but also adds to a buffer for later processing.
///
/// # Safety
/// Both pointers must be valid and point to live GcHeader objects.
#[inline]
pub unsafe fn insertion_barrier_buffered(
    from: *const GcHeader,
    to: *const GcHeader,
    buffer: &WriteBarrierBuffer,
) -> bool {
    if from.is_null() || to.is_null() {
        return false;
    }

    // SAFETY: Caller guarantees pointers are valid
    let from_header = unsafe { &*from };
    let to_header = unsafe { &*to };

    // Dijkstra barrier: if storing white into black, gray the target
    if from_header.mark() == MarkColor::Black && to_header.mark() == MarkColor::White {
        to_header.set_mark(MarkColor::Gray);
        // Also add to buffer for re-scanning
        return buffer.push(to);
    }

    false
}

/// Deletion barrier (Yuasa-style / snapshot-at-the-beginning)
///
/// Called when overwriting or deleting a reference from `from` to `old_value`.
/// Ensures that the old reference is not lost during concurrent marking.
///
/// # Safety
/// Both pointers must be valid (or null) and point to live GcHeader objects.
#[inline]
pub unsafe fn deletion_barrier(from: *const GcHeader, old_value: *const GcHeader) {
    if old_value.is_null() {
        return;
    }

    // SAFETY: Caller guarantees pointer is valid
    let old_header = unsafe { &*old_value };

    // If the old value is white, gray it to prevent premature collection
    // This implements snapshot-at-the-beginning semantics
    if old_header.mark() == MarkColor::White {
        old_header.set_mark(MarkColor::Gray);
    }

    // Also need to check the source
    if !from.is_null() {
        let from_header = unsafe { &*from };
        // If source was black and we're removing a reference,
        // we might need to re-scan the source (not implemented here)
        let _ = from_header; // Used to check mark in more complex implementations
    }
}

/// Generational write barrier
///
/// Called when an old-gen object stores a reference to a young-gen object.
/// Records the cross-generation reference in the remembered set.
///
/// # Safety
/// Both pointers must be valid and point to live GcHeader objects.
/// `is_young_fn` determines if an object is in the young generation.
#[inline]
pub unsafe fn generational_barrier<F>(
    from: *const GcHeader,
    to: *const GcHeader,
    remembered_set: &RememberedSet,
    is_young_fn: F,
) where
    F: Fn(*const GcHeader) -> bool,
{
    if from.is_null() || to.is_null() {
        return;
    }

    // If storing a young pointer into an old object, remember it
    if !is_young_fn(from) && is_young_fn(to) {
        remembered_set.add(from);
    }
}

/// Combined barrier for concurrent generational GC
///
/// Combines insertion barrier with generational tracking.
///
/// # Safety
/// Both pointers must be valid and point to live GcHeader objects.
#[inline]
pub unsafe fn combined_barrier<F>(
    from: *const GcHeader,
    to: *const GcHeader,
    remembered_set: &RememberedSet,
    buffer: &WriteBarrierBuffer,
    is_young_fn: F,
) -> bool
where
    F: Fn(*const GcHeader) -> bool,
{
    if from.is_null() || to.is_null() {
        return false;
    }

    // Generational barrier
    if !is_young_fn(from) && is_young_fn(to) {
        remembered_set.add(from);
    }

    // Insertion barrier with buffering
    // SAFETY: Caller guarantees pointers are valid
    unsafe { insertion_barrier_buffered(from, to, buffer) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::tags;

    #[test]
    fn test_card_table() {
        let base = 0x1000;
        let size = 4096;
        let mut table = CardTable::new(base, size);

        // Initially clean
        assert!(!table.is_dirty(0x1000));
        assert!(!table.is_dirty(0x1200));

        // Mark a card
        table.mark_card(0x1100);
        assert!(table.is_dirty(0x1100));
        assert!(table.is_dirty(0x1000)); // Same card

        // Different card
        assert!(!table.is_dirty(0x1400));

        // Clear
        table.clear();
        assert!(!table.is_dirty(0x1100));
    }

    #[test]
    fn test_write_barrier_buffer() {
        let buffer = WriteBarrierBuffer::with_capacity(3);
        let header1 = GcHeader::new(tags::OBJECT);
        let header2 = GcHeader::new(tags::OBJECT);
        let header3 = GcHeader::new(tags::OBJECT);

        assert!(buffer.is_empty());

        buffer.push(&header1 as *const _);
        buffer.push(&header2 as *const _);
        assert_eq!(buffer.len(), 2);

        // Third push fills the buffer
        let full = buffer.push(&header3 as *const _);
        assert!(full);

        // Drain
        let entries = buffer.drain();
        assert_eq!(entries.len(), 3);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_remembered_set() {
        let rs = RememberedSet::new();
        let header1 = GcHeader::new(tags::OBJECT);
        let header2 = GcHeader::new(tags::OBJECT);

        assert!(rs.is_empty());

        rs.add(&header1 as *const _);
        assert!(rs.contains(&header1 as *const _));
        assert!(!rs.contains(&header2 as *const _));

        rs.add(&header2 as *const _);
        assert_eq!(rs.len(), 2);

        rs.remove(&header1 as *const _);
        assert!(!rs.contains(&header1 as *const _));
        assert!(rs.contains(&header2 as *const _));

        rs.clear();
        assert!(rs.is_empty());
    }

    #[test]
    fn test_insertion_barrier() {
        let from = GcHeader::new(tags::OBJECT);
        let to = GcHeader::new(tags::OBJECT);

        // Set from to black
        from.set_mark(MarkColor::Black);
        assert_eq!(to.mark(), MarkColor::White);

        // Barrier should gray 'to'
        // SAFETY: Both pointers are valid stack references
        unsafe { insertion_barrier(&from as *const _, &to as *const _) };
        assert_eq!(to.mark(), MarkColor::Gray);
    }

    #[test]
    fn test_deletion_barrier() {
        let from = GcHeader::new(tags::OBJECT);
        let old = GcHeader::new(tags::OBJECT);

        assert_eq!(old.mark(), MarkColor::White);

        // Deletion barrier should gray the old value
        // SAFETY: Both pointers are valid stack references
        unsafe { deletion_barrier(&from as *const _, &old as *const _) };
        assert_eq!(old.mark(), MarkColor::Gray);
    }

    #[test]
    fn test_generational_barrier() {
        let rs = RememberedSet::new();
        let old_obj = GcHeader::new(tags::OBJECT);
        let young_obj = GcHeader::new(tags::OBJECT);

        // Simulate: old_obj is old-gen, young_obj is young-gen
        let is_young = |ptr: *const GcHeader| std::ptr::eq(ptr, &young_obj);

        // SAFETY: Both pointers are valid stack references
        unsafe {
            generational_barrier(&old_obj as *const _, &young_obj as *const _, &rs, is_young);
        }

        // old_obj should be in remembered set
        assert!(rs.contains(&old_obj as *const _));
    }

    #[test]
    fn test_dirty_cards_iteration() {
        let base = 0;
        let size = CARD_SIZE * 4;
        let table = CardTable::new(base, size);

        // Mark cards 0 and 2
        table.mark_card(100); // Card 0
        table.mark_card(CARD_SIZE * 2 + 50); // Card 2

        let dirty: Vec<_> = table.dirty_cards().collect();
        assert_eq!(dirty.len(), 2);
        assert_eq!(dirty[0], (0, CARD_SIZE));
        assert_eq!(dirty[1], (CARD_SIZE * 2, CARD_SIZE * 3));
    }
}
