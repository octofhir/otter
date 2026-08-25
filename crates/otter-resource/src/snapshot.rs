//! Deterministic resource-ledger snapshots.
//!
//! # Contents
//! - [`ResourceSnapshot`] contains every class in stable order.
//! - [`ResourceSnapshotEntry`] reports one class's counters and limit.
//!
//! # Invariants
//! - Entry order always matches [`ResourceClass::ALL`].
//! - Snapshots are immutable and contain no shared live-ledger handles.
//!
//! # See also
//! - [`crate::ResourceAccount::snapshot`] captures a snapshot atomically.

use crate::class::{RESOURCE_CLASS_COUNT, ResourceClass};

/// One class entry in a deterministic [`ResourceSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceSnapshotEntry {
    class: ResourceClass,
    current: u64,
    peak: u64,
    rejections: u64,
    limit: Option<u64>,
}

impl ResourceSnapshotEntry {
    pub(crate) const fn new(
        class: ResourceClass,
        current: u64,
        peak: u64,
        rejections: u64,
        limit: Option<u64>,
    ) -> Self {
        Self {
            class,
            current,
            peak,
            rejections,
            limit,
        }
    }

    /// Return the resource class described by this entry.
    #[must_use]
    pub const fn class(&self) -> ResourceClass {
        self.class
    }

    /// Return the amount currently charged.
    #[must_use]
    pub const fn current(&self) -> u64 {
        self.current
    }

    /// Return the largest amount ever charged concurrently.
    #[must_use]
    pub const fn peak(&self) -> u64 {
        self.peak
    }

    /// Return the cumulative number of rejected reserves or commits.
    #[must_use]
    pub const fn rejections(&self) -> u64 {
        self.rejections
    }

    /// Return the class limit, or `None` when the class is unlimited.
    #[must_use]
    pub const fn limit(&self) -> Option<u64> {
        self.limit
    }
}

/// An immutable point-in-time view of every resource class.
///
/// Entries are stored and iterated in [`ResourceClass::ALL`] order, making the
/// snapshot deterministic without allocating or sorting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceSnapshot {
    entries: [ResourceSnapshotEntry; RESOURCE_CLASS_COUNT],
}

impl Default for ResourceSnapshot {
    /// An empty snapshot: every class at zero usage with no limit. The shape
    /// diagnostics carriers embed before their first real capture.
    fn default() -> Self {
        Self {
            entries: ResourceClass::ALL
                .map(|class| ResourceSnapshotEntry::new(class, 0, 0, 0, None)),
        }
    }
}

impl ResourceSnapshot {
    pub(crate) const fn new(entries: [ResourceSnapshotEntry; RESOURCE_CLASS_COUNT]) -> Self {
        Self { entries }
    }

    /// Return the entry for `class` in constant time.
    #[must_use]
    pub const fn get(&self, class: ResourceClass) -> &ResourceSnapshotEntry {
        &self.entries[class.index()]
    }

    /// Return all entries in stable [`ResourceClass::ALL`] order.
    #[must_use]
    pub const fn entries(&self) -> &[ResourceSnapshotEntry; RESOURCE_CLASS_COUNT] {
        &self.entries
    }

    /// Iterate over entries in stable [`ResourceClass::ALL`] order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ResourceSnapshotEntry> {
        self.entries.iter()
    }
}
