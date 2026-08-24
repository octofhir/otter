//! Isolate-local table of external (non-GC) references held by heap bodies.
//!
//! Some bodies point at things the collector does not own and cannot move:
//! Rust function entry addresses, most of all. An opaque in-process heap image
//! stores only their dense indices and shares the exact address list; no raw
//! address is serialized or accepted from bytes.
//!
//! This table gives every such address a small, dense, isolate-local
//! index. A body stores the index; the address is looked up here when
//! it is actually needed. An index is stable for the life of the heap
//! and — because interning happens in bootstrap install order — two
//! heaps built the same way in the same binary agree on every index.
//!
//! It is the same shape as [`crate::trace::TraceTable`]: a registry of
//! external function pointers, filled automatically the first time the
//! GC is asked to record one, owned per heap so no process-global
//! state can bleed one isolate's ordering into another's.
//!
//! # Contents
//!
//! - [`ExternalRefTable`] — the interner.
//! - [`NO_EXTERNAL_REF`] — the reserved "this body holds none" index.
//!
//! # Invariants
//!
//! - Index `0` is [`NO_EXTERNAL_REF`] and never names an address, so a
//!   zeroed body field reads as "absent" without a second flag.
//! - [`ExternalRefTable::intern`] is idempotent: the same address
//!   always returns the same index, and indices are handed out in
//!   ascending first-intern order.
//! - Entries are never removed or reordered. The table only grows.
//!
//! # See also
//!
//! - [`crate::trace::TraceTable`] — the per-tag trace/drop registry
//!   this mirrors.

use std::collections::HashMap;

/// Reserved index meaning "no external reference". A body whose index
/// field is zero holds none.
pub const NO_EXTERNAL_REF: u32 = 0;

/// Isolate-local interner mapping external addresses to dense indices.
#[derive(Debug, Default)]
pub struct ExternalRefTable {
    /// Address at index `i + 1`; index 0 is [`NO_EXTERNAL_REF`].
    addrs: Vec<usize>,
    index_of: HashMap<usize, u32>,
}

impl ExternalRefTable {
    /// Empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            addrs: Vec::new(),
            index_of: HashMap::new(),
        }
    }

    /// Index for `addr`, assigning the next one on first sight.
    ///
    /// Returns [`NO_EXTERNAL_REF`] for a null address so callers can
    /// funnel "maybe an address" through one path.
    pub fn intern(&mut self, addr: usize) -> u32 {
        if addr == 0 {
            return NO_EXTERNAL_REF;
        }
        if let Some(&index) = self.index_of.get(&addr) {
            return index;
        }
        self.addrs.push(addr);
        let index = self.addrs.len() as u32;
        self.index_of.insert(addr, index);
        index
    }

    /// Index already assigned to `addr`, without assigning one.
    #[must_use]
    pub fn lookup(&self, addr: usize) -> Option<u32> {
        if addr == 0 {
            return None;
        }
        self.index_of.get(&addr).copied()
    }

    /// Address behind `index`, or `None` for [`NO_EXTERNAL_REF`] and
    /// for any index this table never handed out.
    #[must_use]
    pub fn address(&self, index: u32) -> Option<usize> {
        if index == NO_EXTERNAL_REF {
            return None;
        }
        self.addrs.get(index as usize - 1).copied()
    }

    /// Rebuild the table from a same-process captured address list, preserving
    /// indices. The table must be empty — restore precedes any interning in a
    /// restored isolate.
    ///
    /// # Panics
    /// When the table already holds entries; that is a caller
    /// sequencing bug, not an input problem.
    pub fn restore_from_addrs(&mut self, addrs: impl IntoIterator<Item = usize>) {
        assert!(
            self.addrs.is_empty(),
            "external-ref restore requires an empty table"
        );
        for addr in addrs {
            self.intern(addr);
        }
    }

    /// Number of distinct addresses interned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.addrs.len()
    }

    /// Whether no address has been interned yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.addrs.is_empty()
    }

    /// Interned addresses in index order (index `i + 1` is `addrs[i]`).
    #[must_use]
    pub fn addresses(&self) -> &[usize] {
        &self.addrs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_is_idempotent_and_dense() {
        let mut table = ExternalRefTable::new();
        let a = table.intern(0x1000);
        let b = table.intern(0x2000);
        assert_eq!((a, b), (1, 2));
        assert_eq!(table.intern(0x1000), a);
        assert_eq!(table.intern(0x2000), b);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn zero_address_is_the_reserved_index() {
        let mut table = ExternalRefTable::new();
        assert_eq!(table.intern(0), NO_EXTERNAL_REF);
        assert_eq!(table.address(NO_EXTERNAL_REF), None);
        assert_eq!(table.lookup(0), None);
        assert!(table.is_empty());
    }

    #[test]
    fn address_round_trips_and_rejects_unknown_indices() {
        let mut table = ExternalRefTable::new();
        let index = table.intern(0xdead_beef);
        assert_eq!(table.address(index), Some(0xdead_beef));
        assert_eq!(table.lookup(0xdead_beef), Some(index));
        assert_eq!(table.address(index + 1), None);
        assert_eq!(table.lookup(0x1234), None);
    }

    #[test]
    fn index_order_follows_first_intern_order() {
        let mut first = ExternalRefTable::new();
        let mut second = ExternalRefTable::new();
        for addr in [0x30, 0x10, 0x20, 0x10] {
            first.intern(addr);
            second.intern(addr);
        }
        assert_eq!(first.addresses(), [0x30, 0x10, 0x20]);
        assert_eq!(first.addresses(), second.addresses());
    }
}
