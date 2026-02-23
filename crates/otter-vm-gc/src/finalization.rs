//! FinalizationRegistry GC support — tracks weak targets for cleanup scheduling.
//!
//! Stores weak references to targets. When a target is collected by the GC,
//! the corresponding entry index is moved to a pending queue for cleanup.

use crate::mark_sweep::GcTraceable;
use crate::object::{GcHeader, MarkColor};
use std::cell::RefCell;

/// A single target registration (just the weak pointer and its entry index).
struct TargetEntry {
    /// Weak pointer to the target's GcHeader (NOT traced)
    target_header: *const GcHeader,
    /// Index into the held values array (stored on the JS wrapper object)
    entry_index: u32,
}

/// Data backing a FinalizationRegistry — tracks weak target → entry index mappings.
///
/// The held values, unregister tokens, and cleanup callback are stored on the
/// JS wrapper object as properties, so they get proper GC tracing automatically.
/// This struct only holds the weak target pointers.
pub struct FinalizationRegistryData {
    /// Registered targets and their entry indices
    entries: RefCell<Vec<TargetEntry>>,
    /// Entry indices pending cleanup (target was collected)
    pending_indices: RefCell<Vec<u32>>,
    /// Next entry index to assign
    next_index: RefCell<u32>,
}

// SAFETY: Single-threaded VM
unsafe impl Send for FinalizationRegistryData {}
unsafe impl Sync for FinalizationRegistryData {}

impl FinalizationRegistryData {
    /// Create a new empty FinalizationRegistryData.
    pub fn new() -> Self {
        Self {
            entries: RefCell::new(Vec::new()),
            pending_indices: RefCell::new(Vec::new()),
            next_index: RefCell::new(0),
        }
    }

    /// Register a target and return the entry index.
    pub fn register(&self, target_header: *const GcHeader) -> u32 {
        let idx = *self.next_index.borrow();
        *self.next_index.borrow_mut() = idx + 1;
        self.entries.borrow_mut().push(TargetEntry {
            target_header,
            entry_index: idx,
        });
        idx
    }

    /// Remove all entries whose entry_index matches entries associated with the given target pointer.
    /// Returns true if any entries were removed.
    pub fn unregister_by_target(&self, target_header: *const GcHeader) -> bool {
        let mut entries = self.entries.borrow_mut();
        let before = entries.len();
        entries.retain(|e| e.target_header != target_header);
        entries.len() != before
    }

    /// Remove entries by entry indices (from an unregister token lookup).
    /// Returns true if any entries were removed.
    pub fn unregister_indices(&self, indices: &[u32]) -> bool {
        let mut entries = self.entries.borrow_mut();
        let before = entries.len();
        entries.retain(|e| !indices.contains(&e.entry_index));
        entries.len() != before
    }

    /// Called during GC sweep: check for dead targets and queue their indices.
    ///
    /// # Safety
    /// Must be called during GC sweep when mark bits are valid.
    pub unsafe fn sweep_dead_targets(&self) {
        let mut entries = self.entries.borrow_mut();
        let mut pending = self.pending_indices.borrow_mut();

        entries.retain(|entry| {
            if entry.target_header.is_null() {
                return false;
            }
            // SAFETY: caller guarantees target_header points to a valid GcHeader during sweep
            let header = unsafe { &*entry.target_header };
            if header.mark() == MarkColor::White {
                // Target is dead — queue for cleanup
                pending.push(entry.entry_index);
                false
            } else {
                true
            }
        });
    }

    /// Drain pending cleanup entry indices.
    pub fn drain_pending(&self) -> Vec<u32> {
        self.pending_indices.borrow_mut().drain(..).collect()
    }

    /// Check if there are pending cleanups.
    pub fn has_pending(&self) -> bool {
        !self.pending_indices.borrow().is_empty()
    }
}

impl Default for FinalizationRegistryData {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FinalizationRegistryData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FinalizationRegistryData")
            .field("entries", &self.entries.borrow().len())
            .field("pending", &self.pending_indices.borrow().len())
            .finish()
    }
}

impl GcTraceable for FinalizationRegistryData {
    /// FinalizationRegistryData does NOT trace targets (they are weak references).
    /// The held values and callback are stored on the JS wrapper and traced there.
    const NEEDS_TRACE: bool = false;

    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {
        // Nothing to trace — all strong references live on the JS wrapper object.
    }
}
