//! Generic persistent roots for host-owned resources.
//!
//! Host integrations sometimes need to keep a JavaScript callback or helper
//! object alive after the native function returns, while the actual host state
//! remains ordinary Rust data. This table gives those integrations opaque root
//! ids instead of letting them store [`Value`] in host objects.
//!
//! # Contents
//! - [`PersistentRootId`] - stable id stored by host resources.
//! - [`PersistentRoots`] - per-isolate root table with free-list reuse.
//!
//! # Invariants
//! - The table stores only VM [`Value`] roots and lives on the isolate.
//! - Host data stores ids, never raw [`Value`] handles.
//! - Callers must remove roots when the host resource closes.
//!
//! # See also
//! - [`crate::timers`] for callback-specific traced state.

use crate::Value;

/// Opaque persistent-root id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PersistentRootId(u32);

impl PersistentRootId {
    /// Raw integer for diagnostics and host-object payloads.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// Rebuild an id from host-object payload data.
    #[must_use]
    pub const fn from_u32(value: u32) -> Self {
        Self(value)
    }
}

/// Per-isolate persistent root table.
#[derive(Debug, Default)]
pub struct PersistentRoots {
    entries: Vec<Option<Value>>,
    free: Vec<u32>,
}

impl PersistentRoots {
    /// Empty root table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `value` and return its root id.
    pub fn insert(&mut self, value: Value) -> PersistentRootId {
        if let Some(idx) = self.free.pop() {
            self.entries[idx as usize] = Some(value);
            PersistentRootId(idx)
        } else {
            let idx = self.entries.len() as u32;
            self.entries.push(Some(value));
            PersistentRootId(idx)
        }
    }

    /// Read a rooted value.
    #[must_use]
    pub fn get(&self, id: PersistentRootId) -> Option<Value> {
        self.entries.get(id.0 as usize).and_then(|slot| *slot)
    }

    /// Remove a rooted value.
    pub fn remove(&mut self, id: PersistentRootId) -> Option<Value> {
        let slot = self.entries.get_mut(id.0 as usize)?;
        let value = slot.take()?;
        self.free.push(id.0);
        Some(value)
    }

    /// Number of live roots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.iter().filter(|entry| entry.is_some()).count()
    }

    /// `true` when the table has no live roots.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    /// Trace every live value root.
    pub(crate) fn trace_gc_slots(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        for value in self.entries.iter().flatten() {
            value.trace_value_slots(visitor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NumberValue;

    #[test]
    fn persistent_roots_insert_remove_and_reuse_slots() {
        let mut roots = PersistentRoots::new();
        assert!(roots.is_empty());

        let first = roots.insert(Value::number(NumberValue::from_i32(1)));
        let second = roots.insert(Value::number(NumberValue::from_i32(2)));
        assert_eq!(roots.len(), 2);
        assert_eq!(
            roots
                .get(first)
                .and_then(Value::as_number)
                .unwrap()
                .as_f64(),
            1.0
        );

        assert!(roots.remove(first).is_some());
        assert!(roots.get(first).is_none());
        assert_eq!(roots.len(), 1);

        let reused = roots.insert(Value::number(NumberValue::from_i32(3)));
        assert_eq!(reused, first);
        assert_eq!(
            roots
                .get(reused)
                .and_then(Value::as_number)
                .unwrap()
                .as_f64(),
            3.0
        );
        assert!(roots.remove(second).is_some());
        assert!(roots.remove(reused).is_some());
        assert!(roots.is_empty());
    }
}
