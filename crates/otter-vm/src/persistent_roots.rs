//! Generic persistent roots for host-owned resources.
//!
//! Host integrations sometimes need to keep a JavaScript callback or helper
//! object alive after the native function returns, while the actual host state
//! remains ordinary Rust data. This table gives those integrations opaque root
//! ids instead of letting them store [`Value`] in host objects.
//!
//! # Contents
//! - [`PersistentRootId`] - generational key stored by host resources.
//! - [`PersistentRoots`] - per-isolate root table with free-list reuse.
//!
//! # Invariants
//! - The table stores only VM [`Value`] roots and lives on the isolate.
//! - Host data stores ids, never raw [`Value`] handles.
//! - A stale key can never read or remove a root that later reused its slot.
//! - Callers must remove roots when the host resource closes.
//!
//! # See also
//! - [`crate::timers`] for callback-specific traced state.

use crate::Value;

/// Opaque generational persistent-root key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PersistentRootId {
    index: u64,
    generation: u64,
}

impl PersistentRootId {
    /// Slot index for diagnostics. It is not sufficient to reconstruct a key.
    #[must_use]
    pub const fn index(self) -> u64 {
        self.index
    }

    /// Generation for diagnostics and owned host DTOs.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Debug)]
struct PersistentRootEntry {
    generation: u64,
    value: Option<Value>,
}

/// Per-isolate persistent root table.
#[derive(Debug, Default)]
pub struct PersistentRoots {
    entries: Vec<PersistentRootEntry>,
    free: Vec<u64>,
}

impl PersistentRoots {
    /// Empty root table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `value` and return its root id.
    pub fn insert(&mut self, value: Value) -> PersistentRootId {
        while let Some(index) = self.free.pop() {
            let entry = &mut self.entries[index as usize];
            let Some(generation) = entry.generation.checked_add(1) else {
                // Permanently retire a slot whose generation space is exhausted.
                continue;
            };
            entry.generation = generation;
            entry.value = Some(value);
            return PersistentRootId { index, generation };
        }

        let index = self.entries.len() as u64;
        let generation = 1;
        self.entries.push(PersistentRootEntry {
            generation,
            value: Some(value),
        });
        PersistentRootId { index, generation }
    }

    /// Read a rooted value.
    #[must_use]
    pub fn get(&self, id: PersistentRootId) -> Option<Value> {
        let entry = self.entries.get(usize::try_from(id.index).ok()?)?;
        (entry.generation == id.generation)
            .then_some(entry.value)
            .flatten()
    }

    /// Remove a rooted value.
    pub fn remove(&mut self, id: PersistentRootId) -> Option<Value> {
        let entry = self.entries.get_mut(usize::try_from(id.index).ok()?)?;
        if entry.generation != id.generation {
            return None;
        }
        let value = entry.value.take()?;
        self.free.push(id.index);
        Some(value)
    }

    /// Number of live roots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.value.is_some())
            .count()
    }

    /// `true` when the table has no live roots.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(|entry| entry.value.is_none())
    }

    /// Trace every live value root.
    pub(crate) fn trace_gc_slots(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        for entry in &self.entries {
            if let Some(value) = &entry.value {
                value.trace_value_slots(visitor);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NumberValue;

    #[test]
    fn persistent_roots_reject_stale_reused_keys() {
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
        assert_eq!(reused.index(), first.index());
        assert_ne!(reused, first);
        assert!(roots.get(first).is_none());
        assert!(roots.remove(first).is_none());
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
