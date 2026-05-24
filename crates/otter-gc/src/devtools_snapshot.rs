//! Chrome DevTools `.heapsnapshot` writer.
//!
//! Emits a JSON document accepted by Chrome DevTools' "Memory"
//! panel. The format is documented at
//! <https://developer.chrome.com/docs/devtools/memory-problems/heap-snapshots>;
//! the on-the-wire schema lives in V8's `src/profiler/heap-snapshot-generator.cc`.
//!
//! # Contents
//!
//! - [`write_heap_snapshot`] — walk the heap, build the snapshot,
//!   serialize it to a `serde_json::Value`.
//! - [`HeapSnapshotJson`] — typed wrapper over the JSON payload
//!   for tests that want to assert structural validity.
//!
//! # Format
//!
//! ```text
//! {
//!   "snapshot": {
//!     "meta": {
//!       "node_fields":  ["type","name","id","self_size","edge_count","trace_node_id","detachedness"],
//!       "node_types":   [["hidden","array","string","object","code","closure","regexp","number","native","synthetic","concatenated string","sliced string","symbol","bigint","object shape"], "string","number","number","number","number","number"],
//!       "edge_fields":  ["type","name_or_index","to_node"],
//!       "edge_types":   [["context","element","property","internal","hidden","shortcut","weak"], "string_or_number","node"],
//!       "trace_function_info_fields": ["function_id","name","script_name","script_id","line","column"]
//!     },
//!     "node_count":   <usize>,
//!     "edge_count":   <usize>,
//!     "trace_function_count": 0
//!   },
//!   "nodes":   [type, name, id, self_size, edge_count, trace_node_id, detachedness, …],
//!   "edges":   [type, name_or_index, to_node, …],
//!   "strings": ["", "Object", "(GC roots)", …]
//! }
//! ```
//!
//! Nodes and edges are flat arrays of integers; the offsets into
//! the `strings` array index name strings.
//!
//! # See also
//!
//! - GC architecture plan §1.2 NF6, §7.1.

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::compressed::RawGc;
use crate::heap::GcHeap;

/// Type-checked wrapper used by tests.
#[derive(Debug)]
pub struct HeapSnapshotJson(pub Value);

/// Safe wrapper over [`write_heap_snapshot`]. The single-mutator
/// VM model already gives the required STW property whenever the
/// caller holds `&GcHeap` and is not on an allocator path —
/// matching the contract documented on
/// [`crate::heap::GcHeap::snapshot`]. Inspector callers should
/// prefer this entry point so the workspace-wide
/// `forbid(unsafe_code)` lint stays clean.
#[must_use]
pub fn chrome_heap_snapshot(heap: &GcHeap) -> HeapSnapshotJson {
    // SAFETY: see the doc-comment above — `&GcHeap` over a
    // non-allocating call is the documented STW-equivalent
    // contract for single-mutator hosts.
    unsafe { write_heap_snapshot(heap) }
}

/// Walk the heap and emit a Chrome-compatible `.heapsnapshot`.
///
/// # Safety
///
/// Must run under STW pause (no concurrent allocation /
/// barriers).
pub unsafe fn write_heap_snapshot(heap: &GcHeap) -> HeapSnapshotJson {
    // 1) First pass: assign node ids.
    // SAFETY: per docstring.
    let mut nodes: Vec<HeaderEntry> = Vec::new();
    unsafe {
        heap.for_each_live_object(|h| {
            let size = (*h).size_bytes() as usize;
            let tag = (*h).type_tag();
            let raw = HeaderEntry {
                offset: header_to_raw(h),
                tag,
                size,
            };
            nodes.push(raw);
        });
    }
    // Map header offset → node id (1-based; 0 reserved for
    // synthetic root).
    let mut id_map: IndexMap<u32, u32> = IndexMap::new();
    for (idx, n) in nodes.iter().enumerate() {
        id_map.insert(n.offset, (idx as u32) + 1);
    }

    // 2) String table — `Strings` accumulates as we go.
    let mut strings = StringTable::default();
    let _ = strings.intern(""); // empty string canonical idx 0
    let _ = strings.intern("(GC roots)");

    // 3) Build node entries (synthetic root + every live header).
    let mut node_arr: Vec<u64> = Vec::new();
    let mut edge_arr: Vec<u64> = Vec::new();
    let mut total_edges = 0usize;

    // Synthetic root node — id 0.
    // [type=synthetic(9), name="(GC roots)", id=0, self_size=0,
    //  edge_count=0, trace_node_id=0, detachedness=0]
    push_node(&mut node_arr, 9, 1, 0, 0, 0, 0, 0);

    // Walk live objects, emit nodes with edges.
    for entry in &nodes {
        let id = *id_map.get(&entry.offset).unwrap();
        let name_idx = strings.intern(&format!("Object#{:02x}", entry.tag)) as u64;
        // Collect edges for this node first so we know the count.
        let mut entry_edges: Vec<(u8, u64, u64)> = Vec::new();
        // SAFETY: STW pause; header registered with trace table.
        unsafe {
            let header_ptr = raw_to_header(entry.offset);
            heap.trace_one(header_ptr, &mut |slot: *mut RawGc| {
                let raw = (*slot).0;
                if raw == 0 {
                    return;
                }
                if let Some(child_id) = id_map.get(&raw) {
                    // edge type = "internal" (3)
                    entry_edges.push((3, 0, *child_id as u64));
                }
            });
        }
        push_node(
            &mut node_arr,
            3, // type: object
            name_idx,
            id as u64,
            entry.size as u64,
            entry_edges.len() as u64,
            0,
            0,
        );
        for (etype, name_or_idx, to_node) in entry_edges {
            // edges are flat: type, name_or_index, to_node.
            // Indexing into nodes is index*7 (one row per node).
            edge_arr.push(etype as u64);
            edge_arr.push(name_or_idx);
            // V8 expects `to_node` to be the BYTE offset of the
            // target node in the flat node array; with 7 fields
            // per node and `to_node` value of `id * 7`.
            edge_arr.push(to_node * 7);
            total_edges += 1;
        }
    }

    // 4) Pack the JSON document.
    let snapshot = json!({
        "snapshot": {
            "meta": {
                "node_fields": ["type", "name", "id", "self_size", "edge_count", "trace_node_id", "detachedness"],
                "node_types": [["hidden","array","string","object","code","closure","regexp","number","native","synthetic","concatenated string","sliced string","symbol","bigint","object shape"], "string","number","number","number","number","number"],
                "edge_fields": ["type","name_or_index","to_node"],
                "edge_types": [["context","element","property","internal","hidden","shortcut","weak"], "string_or_number","node"],
                "trace_function_info_fields": ["function_id","name","script_name","script_id","line","column"]
            },
            "node_count": node_arr.len() / 7,
            "edge_count": total_edges,
            "trace_function_count": 0
        },
        "nodes": node_arr,
        "edges": edge_arr,
        "strings": strings.into_vec(),
    });
    HeapSnapshotJson(snapshot)
}

#[allow(clippy::too_many_arguments)]
fn push_node(
    out: &mut Vec<u64>,
    ty: u64,
    name: u64,
    id: u64,
    self_size: u64,
    edge_count: u64,
    trace_node_id: u64,
    detachedness: u64,
) {
    out.extend_from_slice(&[
        ty,
        name,
        id,
        self_size,
        edge_count,
        trace_node_id,
        detachedness,
    ]);
}

#[derive(Debug, Default)]
struct StringTable {
    map: IndexMap<String, u32>,
}

impl StringTable {
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&idx) = self.map.get(s) {
            return idx;
        }
        let idx = self.map.len() as u32;
        self.map.insert(s.to_owned(), idx);
        idx
    }

    fn into_vec(self) -> Vec<String> {
        let mut v: Vec<(String, u32)> = self.map.into_iter().collect();
        v.sort_by_key(|(_, idx)| *idx);
        v.into_iter().map(|(s, _)| s).collect()
    }
}

#[derive(Clone, Copy)]
struct HeaderEntry {
    offset: u32,
    tag: u8,
    size: usize,
}

fn header_to_raw(h: *mut crate::header::GcHeader) -> u32 {
    let base_addr = crate::compressed::cage_base_addr();
    let addr = h as usize;
    debug_assert!(addr >= base_addr);
    (addr - base_addr) as u32
}

fn raw_to_header(offset: u32) -> *mut crate::header::GcHeader {
    let base = crate::compressed::cage_base();
    // SAFETY: offset was previously emitted by `header_to_raw`
    // for a live cage allocation.
    unsafe { base.add(offset as usize) as *mut crate::header::GcHeader }
}
