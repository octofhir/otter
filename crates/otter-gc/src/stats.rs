//! Per-heap GC counters and per-type allocation breakdown.
//!
//! Wired to make leaks observable before they show up as host
//! OOM. The counters are pure accounting; updating them is on
//! the slow paths only — the alloc fast path increments a small
//! handful of u64s, the per-GC reconciliation runs once per
//! collection. Migration tasks (76+) read these counters to
//! prove a removed `Rc<RefCell<…>>` cycle actually returns to
//! baseline.
//!
//! # Contents
//!
//! - [`GcStats`] — heap-wide aggregate.
//! - [`TypeStats`] — per-`type_tag` row.
//! - [`TYPE_TAG_COUNT`] — fixed table width (matches the trace
//!   table dispatch array).
//!
//! # Invariants
//!
//! - `live_bytes` and `live_objects` reflect the live set as of
//!   the most recent GC reconciliation; between collections
//!   they grow with every alloc and only the mark-sweep /
//!   scavenge passes correct them downward.
//! - `alloc_count_total` is monotone — only allocs increment
//!   it, never GC.
//! - `free_count_total` is derived after each GC as
//!   `alloc_count_total - live_object_count_per_tag`.
//!
//! # See also
//!
//! - GC architecture plan §1.2 NF6, §7 ("Leak diagnosis").
//! - Task 74 — GC stats, heap snapshot, retained-size walker.

/// Number of distinct `type_tag` slots — matches
/// [`crate::trace::TraceTable`]. Keep in sync.
pub const TYPE_TAG_COUNT: usize = 256;

/// Per-`type_tag` row of the [`GcStats`] table.
#[derive(Debug, Default, Clone, Copy)]
pub struct TypeStats {
    /// Bytes occupied by live objects of this type after the
    /// most recent GC reconciliation.
    pub live_bytes: usize,
    /// Total allocations of this type since the heap was created
    /// (monotone — never decremented).
    pub alloc_count_total: u64,
    /// Total objects of this type reclaimed since the heap was
    /// created (derived after each GC).
    pub free_count_total: u64,
}

impl TypeStats {
    /// Zeroed default value, usable in const contexts.
    pub const DEFAULT: Self = Self {
        live_bytes: 0,
        alloc_count_total: 0,
        free_count_total: 0,
    };
}

/// Heap-wide allocation accounting plus a per-`type_tag`
/// breakdown.
///
/// Updated incrementally in [`crate::heap::GcHeap::alloc`] and
/// reconciled after every collection. Read via
/// [`crate::heap::GcHeap::gc_stats`].
#[derive(Clone)]
pub struct GcStats {
    /// Live object count after the most recent GC reconciliation.
    pub live_objects: usize,
    /// Live byte count after the most recent GC reconciliation
    /// (sum of object sizes).
    pub live_bytes: usize,
    /// Per-`type_tag` rows; index by [`crate::trace::Traceable::TYPE_TAG`].
    pub by_type: [TypeStats; TYPE_TAG_COUNT],
    /// Wall-clock duration of the most recent full GC pause, in
    /// milliseconds. `0.0` until the first full GC fires.
    pub last_gc_pause_ms: f32,
    /// Bytes reclaimed by the most recent full GC sweep.
    pub last_gc_reclaimed_bytes: usize,
    /// Number of full GC cycles executed since the heap was
    /// created.
    pub gc_cycles: u64,
}

impl Default for GcStats {
    fn default() -> Self {
        Self {
            live_objects: 0,
            live_bytes: 0,
            by_type: [TypeStats::DEFAULT; TYPE_TAG_COUNT],
            last_gc_pause_ms: 0.0,
            last_gc_reclaimed_bytes: 0,
            gc_cycles: 0,
        }
    }
}

impl std::fmt::Debug for GcStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Emit only non-zero per-type rows so the dump stays
        // readable.
        let mut by_type = Vec::new();
        for (tag, row) in self.by_type.iter().enumerate() {
            if row.live_bytes != 0 || row.alloc_count_total != 0 || row.free_count_total != 0 {
                by_type.push((tag, *row));
            }
        }
        f.debug_struct("GcStats")
            .field("live_objects", &self.live_objects)
            .field("live_bytes", &self.live_bytes)
            .field("last_gc_pause_ms", &self.last_gc_pause_ms)
            .field("last_gc_reclaimed_bytes", &self.last_gc_reclaimed_bytes)
            .field("gc_cycles", &self.gc_cycles)
            .field("by_type_nonzero", &by_type)
            .finish()
    }
}

impl GcStats {
    /// Bump per-tag and aggregate counters for a fresh
    /// allocation of `size_bytes` under `type_tag`.
    ///
    /// Hot-path: `wrapping_add` rather than `saturating_add` —
    /// the counters are `u64` / `usize`, overflow is never
    /// reached in practice (≥ 1.8 × 10¹⁹ allocations) and the
    /// branch-free wrapping form is critical for the alloc fast
    /// path (see `bench_alloc_young_bump` in
    /// `crates/otter-gc/benches`).
    #[inline(always)]
    pub fn record_alloc(&mut self, type_tag: u8, size_bytes: usize) {
        self.live_objects = self.live_objects.wrapping_add(1);
        self.live_bytes = self.live_bytes.wrapping_add(size_bytes);
        // SAFETY of indexing: `type_tag` is a `u8`, range
        // `[0, 256)`; `by_type` has exactly 256 entries.
        let row = &mut self.by_type[type_tag as usize];
        row.live_bytes = row.live_bytes.wrapping_add(size_bytes);
        row.alloc_count_total = row.alloc_count_total.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_alloc_bumps_aggregate_and_per_type_rows() {
        let mut stats = GcStats::default();
        stats.record_alloc(7, 64);
        stats.record_alloc(7, 64);
        stats.record_alloc(9, 32);
        assert_eq!(stats.live_objects, 3);
        assert_eq!(stats.live_bytes, 64 + 64 + 32);
        assert_eq!(stats.by_type[7].live_bytes, 128);
        assert_eq!(stats.by_type[7].alloc_count_total, 2);
        assert_eq!(stats.by_type[9].live_bytes, 32);
        assert_eq!(stats.by_type[9].alloc_count_total, 1);
        assert_eq!(stats.by_type[0].alloc_count_total, 0);
    }

    #[test]
    fn debug_only_includes_nonzero_rows() {
        let mut stats = GcStats::default();
        stats.record_alloc(42, 16);
        let s = format!("{stats:?}");
        assert!(s.contains("live_objects: 1"));
        assert!(s.contains("by_type_nonzero"));
        // Tag 42 should appear; tag 0 should not.
        assert!(s.contains("42"));
    }
}
