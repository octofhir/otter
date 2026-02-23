//! WeakRef GC support — stores a weak reference to a GC-managed object.
//!
//! The key property of `WeakRefCell` is that it does NOT trace its target,
//! allowing the GC to collect the target when no strong references remain.

use crate::mark_sweep::GcTraceable;
use crate::object::GcHeader;
use std::cell::Cell;

/// A weak reference cell that holds a raw pointer to a GC-managed object's header.
/// The target is NOT traced by the GC — if the target becomes unreachable from
/// strong roots, it will be collected and this cell will be cleared.
pub struct WeakRefCell {
    /// Raw pointer to the target's GcHeader (NOT traced, hence weak)
    target_header: Cell<*const GcHeader>,
    /// Whether the target is still alive
    alive: Cell<bool>,
}

// SAFETY: Single-threaded VM — WeakRefCell is only accessed on one thread.
unsafe impl Send for WeakRefCell {}
unsafe impl Sync for WeakRefCell {}

impl WeakRefCell {
    /// Create a new weak reference to the given target header.
    pub fn new(target_header: *const GcHeader) -> Self {
        Self {
            target_header: Cell::new(target_header),
            alive: Cell::new(true),
        }
    }

    /// Get the target header pointer, if still alive.
    pub fn target(&self) -> Option<*const GcHeader> {
        if self.alive.get() {
            let ptr = self.target_header.get();
            if !ptr.is_null() {
                return Some(ptr);
            }
        }
        None
    }

    /// Check if the target is still alive.
    pub fn is_alive(&self) -> bool {
        self.alive.get()
    }

    /// Clear the weak reference (called by GC when target is collected).
    pub fn clear(&self) {
        self.target_header.set(std::ptr::null());
        self.alive.set(false);
    }
}

impl std::fmt::Debug for WeakRefCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeakRefCell")
            .field("alive", &self.alive.get())
            .finish()
    }
}

impl GcTraceable for WeakRefCell {
    /// WeakRefCell does NOT trace its target — this is the whole point of weak references.
    const NEEDS_TRACE: bool = false;

    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {
        // Intentionally empty — weak references don't keep targets alive
    }
}
