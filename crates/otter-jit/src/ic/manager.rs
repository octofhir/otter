//! IC site manager — tracks IC sites per function.
//!
//! Each property access / call / arithmetic site gets an `ICSite`.
//! When a slow-path executes, the manager generates CacheIR and attaches
//! a new stub to the site.

use std::collections::HashMap;

use crate::cache_ir::{CacheIRSequence, ICSite, StubField};

/// Manages all IC sites for one function.
///
/// Keyed by bytecode PC — each property access / call instruction
/// gets its own IC site.
#[derive(Debug, Clone)]
pub struct ICManager {
    /// Map from bytecode PC to IC site.
    sites: HashMap<u32, ICSite>,
}

impl ICManager {
    /// Create an empty IC manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sites: HashMap::new(),
        }
    }

    /// Get or create an IC site for a given bytecode PC.
    pub fn site_for_pc(&mut self, bytecode_pc: u32) -> &mut ICSite {
        self.sites
            .entry(bytecode_pc)
            .or_insert_with(|| ICSite::new(bytecode_pc))
    }

    /// Get an IC site by PC (read-only), if it exists.
    #[must_use]
    pub fn get_site(&self, bytecode_pc: u32) -> Option<&ICSite> {
        self.sites.get(&bytecode_pc)
    }

    /// Record a monomorphic property load hit and attach a CacheIR stub.
    ///
    /// Called from the slow path after a successful property lookup when we
    /// know the shape and slot offset.
    pub fn record_prop_load(
        &mut self,
        bytecode_pc: u32,
        shape_id: u64,
        slot_offset: u32,
    ) {
        let site = self.site_for_pc(bytecode_pc);
        // Don't attach duplicate stubs for the same shape.
        if site.stubs.iter().any(|s| {
            s.fields.iter().any(|f| matches!(f, StubField::Shape(id) if *id == shape_id))
        }) {
            return;
        }
        let stub = CacheIRSequence::monomorphic_prop_load(shape_id, slot_offset);
        site.attach_stub(stub);
    }

    /// Record a monomorphic property store hit.
    pub fn record_prop_store(
        &mut self,
        bytecode_pc: u32,
        shape_id: u64,
        slot_offset: u32,
    ) {
        let site = self.site_for_pc(bytecode_pc);
        if site.stubs.iter().any(|s| {
            s.fields.iter().any(|f| matches!(f, StubField::Shape(id) if *id == shape_id))
        }) {
            return;
        }
        let stub = CacheIRSequence::monomorphic_prop_store(shape_id, slot_offset);
        site.attach_stub(stub);
    }

    /// Number of IC sites.
    #[must_use]
    pub fn site_count(&self) -> usize {
        self.sites.len()
    }

    /// Total number of stubs across all sites.
    #[must_use]
    pub fn total_stubs(&self) -> usize {
        self.sites.values().map(|s| s.stubs.len()).sum()
    }

    /// Iterator over all IC sites.
    pub fn sites(&self) -> impl Iterator<Item = (&u32, &ICSite)> {
        self.sites.iter()
    }
}

impl Default for ICManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_ir::ICSiteState;

    #[test]
    fn test_record_prop_load() {
        let mut mgr = ICManager::new();
        mgr.record_prop_load(10, 42, 16);
        assert_eq!(mgr.site_count(), 1);
        assert_eq!(mgr.total_stubs(), 1);

        let site = mgr.get_site(10).unwrap();
        assert!(site.is_monomorphic());
    }

    #[test]
    fn test_no_duplicate_stubs() {
        let mut mgr = ICManager::new();
        mgr.record_prop_load(10, 42, 16);
        mgr.record_prop_load(10, 42, 16); // Same shape — should not duplicate.
        assert_eq!(mgr.total_stubs(), 1);
    }

    #[test]
    fn test_polymorphic_transition() {
        let mut mgr = ICManager::new();
        mgr.record_prop_load(10, 1, 0);
        mgr.record_prop_load(10, 2, 8);
        mgr.record_prop_load(10, 3, 16);

        let site = mgr.get_site(10).unwrap();
        assert_eq!(site.state, ICSiteState::Polymorphic);
        assert_eq!(site.stubs.len(), 3);
    }
}
