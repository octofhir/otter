//! Per-space heap census: what the bootstrap actually leaves behind.
//!
//! [`crate::snapshot::HeapSnapshot`] answers "what is reachable and
//! what does it retain"; this module answers the cheaper, flatter
//! question used by diagnostics and in-process image sizing: **which pages
//! hold which objects, and how many bytes of each type sit in old space once
//! a runtime has finished building its API surface**. It is a linear
//! walk of the page payloads with no graph construction, so it can
//! run on a fully-built runtime without perturbing it.
//!
//! # Contents
//!
//! - [`TagRow`] — one type tag's object count and byte total.
//! - [`SpaceCensus`] — per-space totals plus its `TagRow` table.
//! - [`HeapCensus`] — old / young / large censuses and the text
//!   renderer.
//! - [`GcHeap::census`] — build one.
//! - [`GcHeap::for_each_live_payload`] — safe typed iteration over
//!   every live body of one type, for callers outside this crate
//!   that need to inspect payload fields (they keep
//!   `forbid(unsafe_code)`).
//!
//! # Invariants
//!
//! - Free-space fillers ([`crate::header::FREE_TAG`]) are counted as
//!   `filler_bytes`, never as live objects; swept and forwarded
//!   headers are skipped entirely. A census therefore reports the
//!   set an opaque image would retain, not the raw page extent.
//! - `object_count` and `live_bytes` equal the sums over `rows`.
//! - Rows are sorted by descending `bytes`, ties broken by ascending
//!   tag, so the rendered table is stable across runs.
//! - [`GcHeap::for_each_live_payload`] hands out `&T` only for
//!   headers whose tag equals `T::TYPE_TAG`; the trace table
//!   guarantees one type per tag.
//!
//! # See also
//!
//! - [`crate::snapshot`] — reachability + retained size.
//! - [`crate::devtools_snapshot`] — Chrome `.heapsnapshot` export.

use std::fmt::Write as _;

use crate::header::FREE_TAG;
use crate::heap::GcHeap;
use crate::page::SpaceKind;
use crate::trace::Traceable;

/// One type tag's contribution to a space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagRow {
    /// `Traceable::TYPE_TAG` of the counted bodies.
    pub type_tag: u8,
    /// Rust type name registered under the tag, or `"?"` when the
    /// tag has no registration (only possible for a body allocated
    /// through a path that never registered its type).
    pub type_name: &'static str,
    /// Live bodies carrying this tag.
    pub object_count: u64,
    /// Sum of `size_bytes` (header + payload) over those bodies.
    pub bytes: u64,
}

/// Census of one space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceCensus {
    /// Which space this row set describes.
    pub space: SpaceKind,
    /// Pages currently owned by the space.
    pub page_count: usize,
    /// Bytes of page payload the space has bump-allocated, live or
    /// not. The gap against `live_bytes + filler_bytes` is swept
    /// corpses awaiting page release.
    pub allocated_bytes: u64,
    /// Live bodies across every tag.
    pub object_count: u64,
    /// Live bytes across every tag.
    pub live_bytes: u64,
    /// Bytes covered by free-space filler headers.
    pub filler_bytes: u64,
    /// Per-tag totals, descending by `bytes`.
    pub rows: Vec<TagRow>,
}

/// Whole-heap census, one entry per space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeapCensus {
    /// Old generation — the set an opaque in-process image captures.
    pub old: SpaceCensus,
    /// Young generation from-space.
    pub young: SpaceCensus,
    /// Large-object space.
    pub large: SpaceCensus,
}

impl HeapCensus {
    /// Render every space as a deterministic text table.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::with_capacity(4096);
        for space in [&self.old, &self.young, &self.large] {
            render_space(&mut out, space);
        }
        out
    }
}

fn render_space(out: &mut String, census: &SpaceCensus) {
    let _ = writeln!(
        out,
        "; {:?} space — pages={} objects={} live_bytes={} filler_bytes={} allocated_bytes={}",
        census.space,
        census.page_count,
        census.object_count,
        census.live_bytes,
        census.filler_bytes,
        census.allocated_bytes,
    );
    if census.rows.is_empty() {
        return;
    }
    let _ = writeln!(out, "  {:>4}  {:>9}  {:>11}  type", "tag", "count", "bytes");
    for row in &census.rows {
        let _ = writeln!(
            out,
            "  {:#04x}  {:>9}  {:>11}  {}",
            row.type_tag,
            row.object_count,
            row.bytes,
            short_type_name(row.type_name),
        );
    }
}

/// Trim a `std::any::type_name` down to its final path segment so
/// the table stays readable; generic arguments are kept.
fn short_type_name(name: &str) -> &str {
    match name.rsplit_once("::") {
        Some((_, leaf)) if !leaf.is_empty() => leaf,
        _ => name,
    }
}

/// Accumulator for one space's walk.
struct SpaceAccumulator {
    counts: [u64; 256],
    bytes: [u64; 256],
    object_count: u64,
    live_bytes: u64,
    filler_bytes: u64,
}

impl SpaceAccumulator {
    fn new() -> Self {
        Self {
            counts: [0; 256],
            bytes: [0; 256],
            object_count: 0,
            live_bytes: 0,
            filler_bytes: 0,
        }
    }

    fn finish(
        self,
        heap: &GcHeap,
        space: SpaceKind,
        page_count: usize,
        allocated: u64,
    ) -> SpaceCensus {
        let mut rows: Vec<TagRow> = (0..256usize)
            .filter(|&tag| self.counts[tag] != 0)
            .map(|tag| TagRow {
                type_tag: tag as u8,
                type_name: heap.trace_table().name(tag as u8).unwrap_or("?"),
                object_count: self.counts[tag],
                bytes: self.bytes[tag],
            })
            .collect();
        rows.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| a.type_tag.cmp(&b.type_tag))
        });
        SpaceCensus {
            space,
            page_count,
            allocated_bytes: allocated,
            object_count: self.object_count,
            live_bytes: self.live_bytes,
            filler_bytes: self.filler_bytes,
            rows,
        }
    }
}

impl GcHeap {
    /// Walk every page and tally live bodies per space and per type
    /// tag.
    ///
    /// Runs under the single-mutator STW-equivalent contract:
    /// holding `&self` while no allocator path is open. The walk
    /// allocates only Rust-side vectors.
    #[must_use]
    pub fn census(&self) -> HeapCensus {
        let mut spaces = self.census_spaces().map(|(space, pages)| {
            let mut acc = SpaceAccumulator::new();
            let mut allocated = 0u64;
            for page in pages {
                allocated += page.header().allocated_bytes as u64;
                // SAFETY: every header up to `bump_cursor` was written by
                // the matching `bump_alloc`; single-mutator STW contract
                // means no allocator path is concurrently advancing it.
                unsafe {
                    page.for_each_object(|header, _| {
                        let tag = (*header).type_tag();
                        let size = (*header).size_bytes() as u64;
                        if tag == FREE_TAG {
                            acc.filler_bytes += size;
                            return;
                        }
                        if (*header).is_swept() || (*header).is_forwarded() {
                            return;
                        }
                        acc.counts[tag as usize] += 1;
                        acc.bytes[tag as usize] += size;
                        acc.object_count += 1;
                        acc.live_bytes += size;
                    });
                }
            }
            (space, acc.finish(self, space, pages.len(), allocated))
        });
        // `census_spaces` yields old, young, large in that order.
        let large = std::mem::replace(&mut spaces[2].1, empty_space(SpaceKind::Large));
        let young = std::mem::replace(&mut spaces[1].1, empty_space(SpaceKind::NewFrom));
        let old = std::mem::replace(&mut spaces[0].1, empty_space(SpaceKind::Old));
        HeapCensus { old, young, large }
    }

    /// Visit every live body whose type tag is `T::TYPE_TAG`,
    /// together with the space it lives in.
    ///
    /// This is the safe payload-inspection door for crates that keep
    /// `forbid(unsafe_code)`: the page walk and the payload cast stay
    /// inside the GC, the caller only sees `&T`. Same STW-equivalent
    /// contract as [`Self::census`] — `visit` must not allocate in
    /// this heap.
    pub fn for_each_live_payload<T, F>(&self, mut visit: F)
    where
        T: Traceable,
        F: FnMut(SpaceKind, &T),
    {
        let want = T::TYPE_TAG;
        for (space, pages) in self.census_spaces() {
            for page in pages {
                // SAFETY: headers up to `bump_cursor` are valid (see
                // `census`). A header tagged `want` precedes exactly one
                // `T` payload — `TraceTable::register` rejects a second
                // type claiming a live tag — so the cast is type-correct.
                unsafe {
                    page.for_each_object(|header, _| {
                        if (*header).type_tag() != want
                            || (*header).is_swept()
                            || (*header).is_forwarded()
                        {
                            return;
                        }
                        let payload = (header as *const u8)
                            .add(std::mem::size_of::<crate::header::GcHeader>())
                            .cast::<T>();
                        visit(space, &*payload);
                    });
                }
            }
        }
    }
}

fn empty_space(space: SpaceKind) -> SpaceCensus {
    SpaceCensus {
        space,
        page_count: 0,
        allocated_bytes: 0,
        object_count: 0,
        live_bytes: 0,
        filler_bytes: 0,
        rows: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressed::CAGE_TEST_LOCK;
    use crate::test_support::OpaqueLeaf;

    #[test]
    fn census_counts_allocated_bodies() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut heap = GcHeap::new().expect("heap");
        for payload in 0..16u64 {
            heap.alloc(OpaqueLeaf { payload }).expect("alloc");
        }
        let census = heap.census();
        let row = census
            .young
            .rows
            .iter()
            .chain(census.old.rows.iter())
            .find(|r| r.type_tag == OpaqueLeaf::TYPE_TAG)
            .expect("OpaqueLeaf row");
        assert_eq!(row.object_count, 16);
        assert!(row.type_name.ends_with("OpaqueLeaf"), "{}", row.type_name);
    }

    #[test]
    fn space_totals_match_row_sums() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut heap = GcHeap::new().expect("heap");
        for payload in 0..64u64 {
            heap.alloc(OpaqueLeaf { payload }).expect("alloc");
        }
        let census = heap.census();
        for space in [&census.old, &census.young, &census.large] {
            let count: u64 = space.rows.iter().map(|r| r.object_count).sum();
            let bytes: u64 = space.rows.iter().map(|r| r.bytes).sum();
            assert_eq!(count, space.object_count, "{:?}", space.space);
            assert_eq!(bytes, space.live_bytes, "{:?}", space.space);
        }
    }

    #[test]
    fn typed_payload_walk_sees_every_body() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut heap = GcHeap::new().expect("heap");
        for payload in 0..32u64 {
            heap.alloc(OpaqueLeaf { payload }).expect("alloc");
        }
        let mut seen = Vec::new();
        heap.for_each_live_payload::<OpaqueLeaf, _>(|_space, leaf| seen.push(leaf.payload));
        seen.sort_unstable();
        assert_eq!(seen, (0..32u64).collect::<Vec<_>>());
    }
}
