//! Ephemeron registry support for weak JS collections.
//!
//! The collector owns only type-erased weak-collection headers here.
//! VM-specific payload semantics stay in `otter-vm`: after the normal
//! root mark, the VM walks these registered tables, marks values whose
//! keys are already live, and prunes dead-key entries before heap sweep.
//!
//! # Contents
//! - [`EphemeronRegistry`] — per-heap list of weak collection tables.
//!
//! # Invariants
//! - Registry entries are `RawGc` table handles, not keys or values.
//! - The registry never traces entries as strong roots.
//! - Dead tables are pruned after marking and before raw heap sweep.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-weakmap-objects>
//! - <https://tc39.es/ecma262/#sec-weakset-objects>

use crate::compressed::RawGc;

/// Per-heap registry of GC-managed weak collection payloads.
#[derive(Debug, Default)]
pub struct EphemeronRegistry {
    tables: Vec<RawGc>,
}

impl EphemeronRegistry {
    /// Register a weak collection table for post-mark ephemeron work.
    pub fn register(&mut self, table: RawGc) {
        if table.is_null() || self.tables.contains(&table) {
            return;
        }
        self.tables.push(table);
    }

    /// Snapshot table handles. The VM may inspect this while mutating
    /// table payloads without borrowing the registry.
    #[must_use]
    pub fn snapshot(&self) -> Vec<RawGc> {
        self.tables.clone()
    }

    /// Retain only tables that are still live after the mark phase.
    pub fn retain_marked(&mut self, mut is_marked: impl FnMut(RawGc) -> bool) {
        self.tables.retain(|raw| is_marked(*raw));
    }

    /// Mutable raw slots for young-generation relocation.
    ///
    /// These slots are not roots. The scavenger rewrites a slot only when the
    /// table was already forwarded through an ordinary strong root; otherwise
    /// it nulls the entry so the registry can prune it.
    pub fn handle_slots(&mut self) -> Vec<*mut RawGc> {
        self.tables
            .iter_mut()
            .map(|raw| raw as *mut RawGc)
            .collect()
    }

    /// Drop entries nulled by a young-generation collection.
    pub fn retain_non_null(&mut self) {
        self.tables.retain(|raw| !raw.is_null());
    }

    /// Number of registered tables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}
