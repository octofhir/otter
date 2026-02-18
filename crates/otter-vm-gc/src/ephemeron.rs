//! Ephemeron tables for weak key-value mappings
//!
//! Ephemerons are a GC primitive that enables proper weak collection semantics.
//! An ephemeron (key, value) entry is retained ONLY if the key is reachable
//! from roots. When the key becomes unreachable, the entry is collected.
//!
//! This is essential for implementing WeakMap and WeakSet per ECMAScript spec.
//!
//! ## Two-Pass Marking
//!
//! Ephemeron marking requires fixpoint iteration:
//! 1. **Phase 1**: Mark from roots (standard marking)
//! 2. **Phase 2**: For each ephemeron:
//!    - If key is marked (black), mark the value
//!    - If key is unmarked (white), leave value unmarked
//! 3. Repeat Phase 2 until no new values are marked (fixpoint)
//!
//! This ensures proper transitive marking through ephemeron chains.
//!
//! ## Example
//!
//! ```ignore
//! let table = EphemeronTable::new();
//! let key = GcRef::new(JsObject::new(...));
//! let value = Value::int32(42);
//!
//! table.set(key.header(), value);
//!
//! // Later, if key becomes unreachable and GC runs:
//! // - Entry is automatically removed
//! // - value is collected (if not reachable elsewhere)
//! ```

use crate::object::GcHeader;
use rustc_hash::FxHashMap;
use std::cell::RefCell;

/// Ephemeron entry: (key, value) where value is only live if key is live
#[derive(Debug, Clone)]
struct EphemeronEntry {
    /// Pointer to key's GC header (for identity comparison)
    key_ptr: *const GcHeader,
    /// The value (kept alive only if key is alive)
    /// Stored as raw bytes to avoid type-erasing issues
    value: Vec<u8>,
    /// Type-specific drop function
    drop_fn: Option<unsafe fn(*mut u8)>,
}

// SAFETY: EphemeronEntry is only accessed from the single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
unsafe impl Send for EphemeronEntry {}
unsafe impl Sync for EphemeronEntry {}

/// Ephemeron table for WeakMap/WeakSet implementation
///
/// This data structure provides weak key semantics:
/// - Keys are tracked by pointer identity (address)
/// - Values are retained ONLY while their keys are reachable
/// - Automatic cleanup during GC sweep
pub struct EphemeronTable {
    /// Entries indexed by key pointer (as usize for hashing)
    entries: RefCell<FxHashMap<usize, EphemeronEntry>>,
}

// SAFETY: EphemeronTable is only accessed from the single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
unsafe impl Send for EphemeronTable {}
unsafe impl Sync for EphemeronTable {}

impl std::fmt::Debug for EphemeronTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = self.entries.borrow();
        f.debug_struct("EphemeronTable")
            .field("entry_count", &entries.len())
            .finish()
    }
}

impl EphemeronTable {
    /// Create a new empty ephemeron table
    pub fn new() -> Self {
        Self {
            entries: RefCell::new(FxHashMap::default()),
        }
    }

    /// Set or update an entry
    ///
    /// # Arguments
    /// - `key`: GC header pointer of the key object
    /// - `value_bytes`: Serialized value bytes
    /// - `drop_fn`: Optional drop function for the value
    ///
    /// # Safety
    /// - `key` must point to a valid, live GcHeader
    /// - `value_bytes` must be valid for the type represented
    /// - `drop_fn` must correctly deallocate value_bytes if provided
    pub unsafe fn set_raw(
        &self,
        key: *const GcHeader,
        value_bytes: Vec<u8>,
        drop_fn: Option<unsafe fn(*mut u8)>,
    ) {
        let key_addr = key as usize;
        let entry = EphemeronEntry {
            key_ptr: key,
            value: value_bytes,
            drop_fn,
        };

        self.entries.borrow_mut().insert(key_addr, entry);
    }

    /// Get value for a key (if key exists and is still alive)
    ///
    /// Returns None if:
    /// - Key not found in table
    /// - Key has been collected
    ///
    /// # Safety
    /// - `key` must point to a valid, live GcHeader
    /// - Returned bytes must be interpreted as the correct type
    pub unsafe fn get_raw(&self, key: *const GcHeader) -> Option<Vec<u8>> {
        let key_addr = key as usize;
        self.entries
            .borrow()
            .get(&key_addr)
            .map(|entry| entry.value.clone())
    }

    /// Check if a key exists in the table
    ///
    /// # Safety
    /// - `key` must point to a valid GcHeader (may be dead)
    pub unsafe fn has(&self, key: *const GcHeader) -> bool {
        let key_addr = key as usize;
        self.entries.borrow().contains_key(&key_addr)
    }

    /// Remove an entry by key
    ///
    /// Returns true if the entry existed and was removed.
    ///
    /// # Safety
    /// - `key` must point to a valid GcHeader (may be dead)
    pub unsafe fn delete(&self, key: *const GcHeader) -> bool {
        let key_addr = key as usize;
        let mut entries = self.entries.borrow_mut();

        if let Some(entry) = entries.remove(&key_addr) {
            // Call drop_fn if provided
            if let Some(drop_fn) = entry.drop_fn {
                unsafe {
                    drop_fn(entry.value.as_ptr() as *mut u8);
                }
            }
            true
        } else {
            false
        }
    }

    /// Trace live ephemeron entries during GC mark phase
    ///
    /// This is the core of ephemeron semantics. For each entry:
    /// - If key is marked (black), mark the value via the tracer
    /// - If key is unmarked (white), skip the value
    ///
    /// Returns the number of newly marked values (for fixpoint detection).
    ///
    /// # Arguments
    /// - `tracer`: Callback to mark GC headers within values
    ///
    /// # Safety
    /// - Must be called during GC mark phase
    /// - `tracer` must correctly mark all GC references in values
    pub unsafe fn trace_live_entries(&self, tracer: &mut dyn FnMut(*const GcHeader)) -> usize {
        let entries = self.entries.borrow();
        let mut newly_marked = 0;

        for entry in entries.values() {
            // Check if key is marked (black)
            if !entry.key_ptr.is_null() {
                unsafe {
                    let key_header = &*entry.key_ptr;

                    // If key is black (marked), we can trace the value
                    if key_header.mark() == crate::object::MarkColor::Black {
                        // Trace the value (call tracer on any GC headers it contains)
                        // For now, we assume value is a single GC header pointer
                        // In a full implementation, we'd need type-specific tracing
                        if entry.value.len() >= std::mem::size_of::<*const GcHeader>() {
                            let value_header_ptr =
                                *(entry.value.as_ptr() as *const *const GcHeader);
                            if !value_header_ptr.is_null() {
                                let value_header = &*value_header_ptr;

                                // Only count as newly marked if it was white before
                                if value_header.mark() == crate::object::MarkColor::White {
                                    newly_marked += 1;
                                }

                                tracer(value_header_ptr);
                            }
                        }
                    }
                }
            }
        }

        newly_marked
    }

    /// Sweep dead ephemeron entries after GC mark phase
    ///
    /// Removes entries whose keys are unmarked (white).
    /// This should be called after marking completes.
    ///
    /// Returns the number of entries removed.
    ///
    /// # Safety
    /// - Must be called during GC sweep phase
    /// - All live keys must already be marked
    pub unsafe fn sweep(&self) -> usize {
        let mut entries = self.entries.borrow_mut();
        let initial_count = entries.len();

        // Partition: keep entries with live keys, drop entries with dead keys
        entries.retain(|_key_addr, entry| {
            if entry.key_ptr.is_null() {
                return false;
            }

            unsafe {
                let key_header = &*entry.key_ptr;

                // Keep entry if key is marked (black)
                let should_keep = key_header.mark() == crate::object::MarkColor::Black;

                // If we're removing the entry, call drop_fn
                if !should_keep && let Some(drop_fn) = entry.drop_fn {
                    drop_fn(entry.value.as_ptr() as *mut u8);
                }

                should_keep
            }
        });

        initial_count - entries.len()
    }

    /// Get the number of entries in the table
    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    /// Check if the table is empty
    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }

    /// Clear all entries (for testing or table reset)
    pub fn clear(&self) {
        let mut entries = self.entries.borrow_mut();

        // Call drop_fn for each entry
        for (_key, entry) in entries.drain() {
            if let Some(drop_fn) = entry.drop_fn {
                unsafe {
                    drop_fn(entry.value.as_ptr() as *mut u8);
                }
            }
        }
    }
}

impl Default for EphemeronTable {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EphemeronTable {
    fn drop(&mut self) {
        // Clean up all entries
        self.clear();
    }
}

impl crate::mark_sweep::GcTraceable for EphemeronTable {
    const NEEDS_TRACE: bool = true;

    fn trace(&self, tracer: &mut dyn FnMut(*const GcHeader)) {
        // Ephemerons are traced specially during GC mark phase
        // This method is called to mark the table itself, but the actual
        // ephemeron semantics are handled by trace_live_entries()
        unsafe {
            let _ = self.trace_live_entries(tracer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ephemeron_table_new() {
        let table = EphemeronTable::new();
        assert_eq!(table.len(), 0);
        assert!(table.is_empty());
    }

    #[test]
    fn test_ephemeron_table_set_get() {
        let table = EphemeronTable::new();

        // Create a dummy GC header
        let key_header = GcHeader::new(0);
        let key_ptr = &key_header as *const GcHeader;

        // Create a value (just a u64 for testing)
        let value: u64 = 42;
        let value_bytes = value.to_le_bytes().to_vec();

        unsafe {
            table.set_raw(key_ptr, value_bytes.clone(), None);
        }

        assert_eq!(table.len(), 1);
        assert!(unsafe { table.has(key_ptr) });

        let retrieved = unsafe { table.get_raw(key_ptr) };
        assert_eq!(retrieved, Some(value_bytes));
    }

    #[test]
    fn test_ephemeron_table_delete() {
        let table = EphemeronTable::new();

        let key_header = GcHeader::new(0);
        let key_ptr = &key_header as *const GcHeader;

        let value: u64 = 42;
        let value_bytes = value.to_le_bytes().to_vec();

        unsafe {
            table.set_raw(key_ptr, value_bytes, None);
            assert_eq!(table.len(), 1);

            let deleted = table.delete(key_ptr);
            assert!(deleted);
            assert_eq!(table.len(), 0);

            // Second delete should return false
            let deleted_again = table.delete(key_ptr);
            assert!(!deleted_again);
        }
    }

    #[test]
    fn test_ephemeron_table_clear() {
        let table = EphemeronTable::new();

        // Add multiple entries
        for i in 0..10 {
            let key_header = GcHeader::new(i);
            let key_ptr = Box::leak(Box::new(key_header)) as *const GcHeader;
            let value_bytes = vec![i as u8];

            unsafe {
                table.set_raw(key_ptr, value_bytes, None);
            }
        }

        assert_eq!(table.len(), 10);

        table.clear();

        assert_eq!(table.len(), 0);
        assert!(table.is_empty());
    }
}
