//! Heap snapshot + retained-size walker.
//!
//! Produces an in-memory snapshot of the live object graph plus
//! the caller-supplied root set. The snapshot is the input the
//! migration tasks (76+) need to assert "this cycle returns to
//! baseline". Distinct from
//! [`crate::devtools_snapshot::write_heap_snapshot`], which is
//! the Chrome DevTools `.heapsnapshot` JSON exporter — that
//! writer is for production debugging; this snapshot is for
//! Rust-side assertions and for computing per-root retained
//! size.
//!
//! # Contents
//!
//! - [`SnapshotObject`] — one entry per live header.
//! - [`HeapSnapshot`] — the snapshot itself + retained-size
//!   walker.
//! - [`crate::heap::GcHeap::snapshot`] — entry point.
//!
//! # Algorithm
//!
//! Construction is one STW pass over the heap:
//!
//! 1. Walk every live object, assign a sequential index, record
//!    `(type_tag, self_size, raw)`.
//! 2. Trace each object once, mapping each child slot's compressed
//!    offset back to its index — emit a directed edge.
//!
//! [`HeapSnapshot::retained_size`] then walks the directed graph:
//! the **exclusive retained size** of root `R` is the sum of
//! `self_size` over objects only reachable from `R`. With one
//! root, this collapses to the total reachable byte count;
//! shared subgraphs between two roots are exclusive to neither
//! (matches V8 DevTools terminology).
//!
//! # See also
//!
//! - GC architecture plan §7 ("Leak diagnosis"), §1.2 NF6.
//! - V8 DevTools "retained size" definition:
//!   <https://developer.chrome.com/docs/devtools/memory-problems/heap-snapshots#retained_size>
//! - Task 74 — GC stats, heap snapshot, retained-size walker.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::compressed::{RawGc, cage_base};
use crate::header::GcHeader;
use crate::heap::GcHeap;

/// One entry per live object in [`HeapSnapshot::objects`].
#[derive(Debug, Clone, Copy)]
pub struct SnapshotObject {
    /// `type_tag` from the object's [`GcHeader`].
    pub type_tag: u8,
    /// Total allocation size in bytes (header + payload).
    pub self_size: usize,
    /// Compressed offset of the object's header inside the cage.
    pub raw: RawGc,
}

/// Snapshot of the live object graph plus the root set used to
/// reach it.
///
/// Construct via [`crate::heap::GcHeap::snapshot`].
#[derive(Debug, Clone)]
pub struct HeapSnapshot {
    /// One entry per live object.
    pub objects: Vec<SnapshotObject>,
    /// Caller-supplied roots — copies of the root slot
    /// contents.
    pub roots: Vec<RawGc>,
    /// Directed edges (parent → child) — one entry per
    /// outgoing reference per object.
    pub edges: Vec<(RawGc, RawGc)>,
    /// `RawGc.0` → index in `objects`. Built during construction
    /// and read by [`Self::retained_size`].
    index: HashMap<u32, usize>,
    /// `parent_idx → child_idx, …` — adjacency list mirroring
    /// `edges`.
    adjacency: Vec<Vec<usize>>,
}

impl HeapSnapshot {
    /// Build a snapshot from the raw walking output. Intended for
    /// internal use by [`crate::heap::GcHeap::snapshot`]; exposed
    /// `pub(crate)` so the heap module can construct one without
    /// re-parsing private fields.
    pub(crate) fn build(
        objects: Vec<SnapshotObject>,
        roots: Vec<RawGc>,
        edges: Vec<(RawGc, RawGc)>,
        index: HashMap<u32, usize>,
        adjacency: Vec<Vec<usize>>,
    ) -> Self {
        Self {
            objects,
            roots,
            edges,
            index,
            adjacency,
        }
    }

    /// Look up the object index for a [`RawGc`]. `None` if the
    /// pointer is null or not present in the snapshot.
    #[must_use]
    pub fn index_of(&self, gc: RawGc) -> Option<usize> {
        if gc.is_null() {
            return None;
        }
        self.index.get(&gc.0).copied()
    }

    /// Exclusive retained size of `gc`: the sum of `self_size`
    /// over every live object that is reachable from `gc` and
    /// **not** reachable from any other root in
    /// [`Self::roots`]. Returns `0` when `gc` is null or absent
    /// from the snapshot.
    #[must_use]
    pub fn retained_size(&self, gc: RawGc) -> usize {
        let Some(start) = self.index_of(gc) else {
            return 0;
        };
        let reachable_from_gc = self.bfs(start);
        let mut reachable_from_others: HashSet<usize> = HashSet::new();
        for &other in &self.roots {
            if other == gc {
                continue;
            }
            if let Some(other_idx) = self.index_of(other) {
                let visited = self.bfs(other_idx);
                reachable_from_others.extend(visited);
            }
        }
        reachable_from_gc
            .iter()
            .filter(|i| !reachable_from_others.contains(i))
            .map(|&i| self.objects[i].self_size)
            .sum()
    }

    /// Total live bytes grouped by `type_tag`. Returns a fixed
    /// 256-entry array indexed by [`crate::trace::Traceable::TYPE_TAG`].
    #[must_use]
    pub fn group_by_type(&self) -> [usize; 256] {
        let mut out = [0usize; 256];
        for obj in &self.objects {
            out[obj.type_tag as usize] = out[obj.type_tag as usize].saturating_add(obj.self_size);
        }
        out
    }

    /// BFS from a single starting object index.
    fn bfs(&self, start: usize) -> HashSet<usize> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        visited.insert(start);
        queue.push_back(start);
        while let Some(node) = queue.pop_front() {
            for &child in &self.adjacency[node] {
                if visited.insert(child) {
                    queue.push_back(child);
                }
            }
        }
        visited
    }
}

impl GcHeap {
    /// Walk the heap under STW pause and produce a snapshot
    /// rooted at `roots`.
    ///
    /// `roots` are typically the values of every live root slot
    /// the caller wants to attribute retained size to —
    /// e.g. a single test handle, or the union of all handle
    /// stack entries plus the global handle table.
    ///
    /// # Safety contract (internal STW)
    ///
    /// Construction reads every live header and traces each
    /// object once via [`Self::trace_one`]; both routines
    /// require an STW pause. The single-mutator GC model means
    /// holding `&self` while no allocator path runs is
    /// sufficient.
    #[must_use]
    pub fn snapshot(&self, roots: &[RawGc]) -> HeapSnapshot {
        let mut objects: Vec<SnapshotObject> = Vec::new();
        let mut index: HashMap<u32, usize> = HashMap::new();
        // 1) Index every live header.
        // SAFETY: STW pause — no concurrent allocation.
        unsafe {
            self.for_each_live_object(|h| {
                let raw = RawGc(header_offset(h));
                let type_tag = (*h).type_tag();
                let self_size = (*h).size_bytes() as usize;
                let idx = objects.len();
                index.insert(raw.0, idx);
                objects.push(SnapshotObject {
                    type_tag,
                    self_size,
                    raw,
                });
            });
        }
        // 2) Trace each object, accumulating edges + adjacency.
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); objects.len()];
        let mut edges: Vec<(RawGc, RawGc)> = Vec::new();
        for parent_idx in 0..objects.len() {
            let parent_raw = objects[parent_idx].raw;
            let header_ptr = parent_raw.as_header_ptr();
            // SAFETY: STW pause; type tag is registered
            // (otherwise the object would not have been
            // traceable — alloc<T> registers `T` lazily).
            unsafe {
                self.trace_one(header_ptr, &mut |slot: *mut RawGc| {
                    let child = *slot;
                    if child.is_null() {
                        return;
                    }
                    if let Some(&child_idx) = index.get(&child.0) {
                        adjacency[parent_idx].push(child_idx);
                        edges.push((parent_raw, child));
                    }
                });
            }
        }
        HeapSnapshot::build(objects, roots.to_vec(), edges, index, adjacency)
    }
}

/// Convert a live header pointer into its compressed cage
/// offset.
fn header_offset(h: *mut GcHeader) -> u32 {
    let base_addr = cage_base() as usize;
    let addr = h as usize;
    debug_assert!(addr >= base_addr, "header lies before cage base");
    (addr - base_addr) as u32
}
