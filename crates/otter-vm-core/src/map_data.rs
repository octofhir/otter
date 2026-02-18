//! Backing data structures for Map and Set (ES2023 §23.1, §23.2).
//!
//! Uses proper SameValueZero semantics via `MapKey`, insertion-ordered storage
//! with tombstone-based deletion for live iteration.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::intrinsics_impl::helpers::MapKey;
use crate::value::Value;

// ============================================================================
// MapData
// ============================================================================

/// Internal storage for a JavaScript `Map`.
///
/// Entries are stored in a `Vec` in insertion order. Deleted entries become
/// `None` (tombstones) so that live iterators correctly skip them and still
/// see entries appended after iterator creation.
///
/// A separate `HashMap` provides O(1) key→index lookup.
pub struct MapData {
    inner: RefCell<MapDataInner>,
}

// SAFETY: MapData is only accessed from the single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
unsafe impl Send for MapData {}
unsafe impl Sync for MapData {}

struct MapDataInner {
    /// Insertion-ordered entries. `None` = tombstone (deleted).
    entries: Vec<Option<(MapKey, Value)>>,
    /// Key → index in `entries` for O(1) lookup.
    index: HashMap<MapKey, usize>,
    /// Count of live (non-None) entries.
    size: usize,
}

impl Default for MapData {
    fn default() -> Self {
        Self::new()
    }
}

impl MapData {
    /// Create an empty MapData.
    pub fn new() -> Self {
        Self {
            inner: RefCell::new(MapDataInner {
                entries: Vec::new(),
                index: HashMap::new(),
                size: 0,
            }),
        }
    }

    /// Number of live entries.
    pub fn size(&self) -> usize {
        self.inner.borrow().size
    }

    /// Get the value associated with `key`, or `None`.
    pub fn get(&self, key: &MapKey) -> Option<Value> {
        let inner = self.inner.borrow();
        if let Some(&idx) = inner.index.get(key)
            && let Some(Some((_, v))) = inner.entries.get(idx)
        {
            return Some(v.clone());
        }
        None
    }

    /// If `key` exists, return its value. Otherwise insert `key` → `default_value`
    /// and return `default_value`. Used by `Map.prototype.getOrInsert`.
    pub fn get_or_insert(&self, key: MapKey, default_value: Value) -> Value {
        let mut inner = self.inner.borrow_mut();
        if let Some(&idx) = inner.index.get(&key) {
            if let Some(Some((_, v))) = inner.entries.get(idx) {
                return v.clone();
            }
        }
        // Key not found — insert
        let idx = inner.entries.len();
        inner.index.insert(key.clone(), idx);
        inner.entries.push(Some((key, default_value.clone())));
        inner.size += 1;
        default_value
    }

    /// Returns `true` if `key` exists.
    pub fn has(&self, key: &MapKey) -> bool {
        self.inner.borrow().index.contains_key(key)
    }

    /// Insert or update `key` → `value`. Returns `true` if this was an update.
    pub fn set(&self, key: MapKey, value: Value) -> bool {
        let mut inner = self.inner.borrow_mut();
        if let Some(&idx) = inner.index.get(&key) {
            // Update existing entry in-place (preserves insertion order)
            inner.entries[idx] = Some((key, value));
            true
        } else {
            // Append new entry
            let idx = inner.entries.len();
            inner.index.insert(key.clone(), idx);
            inner.entries.push(Some((key, value)));
            inner.size += 1;
            false
        }
    }

    /// Delete `key`. Returns `true` if it existed.
    pub fn delete(&self, key: &MapKey) -> bool {
        let mut inner = self.inner.borrow_mut();
        if let Some(idx) = inner.index.remove(key) {
            inner.entries[idx] = None; // tombstone
            inner.size -= 1;
            true
        } else {
            false
        }
    }

    /// Remove all entries (iterators in progress will see "done").
    pub fn clear(&self) {
        let mut inner = self.inner.borrow_mut();
        for entry in inner.entries.iter_mut() {
            *entry = None;
        }
        inner.index.clear();
        inner.size = 0;
    }

    /// Read entry at `position` for iterator advancement.
    /// Returns `(key, value)` at that index, or `None` if tombstone/out-of-bounds.
    pub fn entry_at(&self, position: usize) -> Option<(Value, Value)> {
        let inner = self.inner.borrow();
        match inner.entries.get(position) {
            Some(Some((k, v))) => Some((k.value().clone(), v.clone())),
            _ => None,
        }
    }

    /// Current length of the entries vector (including tombstones).
    /// Used by iterators to know when they've exhausted all entries.
    pub fn entries_len(&self) -> usize {
        self.inner.borrow().entries.len()
    }

    /// Collect all live entries for forEach iteration.
    /// The borrow is released before callbacks run, enabling re-entrant operations.
    pub fn for_each_entries(&self) -> Vec<(Value, Value)> {
        let inner = self.inner.borrow();
        let mut result = Vec::with_capacity(inner.size);
        for (k, v) in inner.entries.iter().flatten() {
            result.push((k.value().clone(), v.clone()));
        }
        result
    }
}

impl std::fmt::Debug for MapData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.borrow();
        write!(f, "MapData(size={})", inner.size)
    }
}

impl otter_vm_gc::GcTraceable for MapData {
    const NEEDS_TRACE: bool = true;

    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        let inner = self.inner.borrow();
        for (k, v) in inner.entries.iter().flatten() {
            k.value().trace(tracer);
            v.trace(tracer);
        }
        for key in inner.index.keys() {
            key.value().trace(tracer);
        }
    }
}

// ============================================================================
// SetData
// ============================================================================

/// Internal storage for a JavaScript `Set`.
///
/// Same tombstone-based design as `MapData`, but stores only keys (no values).
pub struct SetData {
    inner: RefCell<SetDataInner>,
}

// SAFETY: SetData is only accessed from the single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
unsafe impl Send for SetData {}
unsafe impl Sync for SetData {}

struct SetDataInner {
    entries: Vec<Option<MapKey>>,
    index: HashMap<MapKey, usize>,
    size: usize,
}

impl Default for SetData {
    fn default() -> Self {
        Self::new()
    }
}

impl SetData {
    /// Create an empty SetData.
    pub fn new() -> Self {
        Self {
            inner: RefCell::new(SetDataInner {
                entries: Vec::new(),
                index: HashMap::new(),
                size: 0,
            }),
        }
    }

    /// Number of live entries.
    pub fn size(&self) -> usize {
        self.inner.borrow().size
    }

    /// Returns `true` if `key` exists.
    pub fn has(&self, key: &MapKey) -> bool {
        self.inner.borrow().index.contains_key(key)
    }

    /// Add a value. Returns `true` if already present (no-op).
    pub fn add(&self, key: MapKey) -> bool {
        let mut inner = self.inner.borrow_mut();
        if inner.index.contains_key(&key) {
            return true; // already present
        }
        let idx = inner.entries.len();
        inner.index.insert(key.clone(), idx);
        inner.entries.push(Some(key));
        inner.size += 1;
        false
    }

    /// Delete `key`. Returns `true` if it existed.
    pub fn delete(&self, key: &MapKey) -> bool {
        let mut inner = self.inner.borrow_mut();
        if let Some(idx) = inner.index.remove(key) {
            inner.entries[idx] = None;
            inner.size -= 1;
            true
        } else {
            false
        }
    }

    /// Remove all entries.
    pub fn clear(&self) {
        let mut inner = self.inner.borrow_mut();
        for entry in inner.entries.iter_mut() {
            *entry = None;
        }
        inner.index.clear();
        inner.size = 0;
    }

    /// Read entry at `position` for iterator advancement.
    pub fn entry_at(&self, position: usize) -> Option<Value> {
        let inner = self.inner.borrow();
        match inner.entries.get(position) {
            Some(Some(k)) => Some(k.value().clone()),
            _ => None,
        }
    }

    /// Current length of the entries vector (including tombstones).
    pub fn entries_len(&self) -> usize {
        self.inner.borrow().entries.len()
    }

    /// Collect all live entries for forEach iteration.
    pub fn for_each_entries(&self) -> Vec<Value> {
        let inner = self.inner.borrow();
        let mut result = Vec::with_capacity(inner.size);
        for k in inner.entries.iter().flatten() {
            result.push(k.value().clone());
        }
        result
    }
}

impl std::fmt::Debug for SetData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.borrow();
        write!(f, "SetData(size={})", inner.size)
    }
}

impl otter_vm_gc::GcTraceable for SetData {
    const NEEDS_TRACE: bool = true;

    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        let inner = self.inner.borrow();
        for k in inner.entries.iter().flatten() {
            k.value().trace(tracer);
        }
        for key in inner.index.keys() {
            key.value().trace(tracer);
        }
    }
}
