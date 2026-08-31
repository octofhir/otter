//! Chrome DevTools `.heapsnapshot` writer — structural validity
//! check.

use otter_gc::devtools_snapshot::write_heap_snapshot;
use otter_gc::raw::RawGc;
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{Gc, GcHeap, HandleScope};

struct Cell {
    next: Gc<Cell>,
}
impl Traceable for Cell {
    const TYPE_TAG: u8 = 0xA0;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        unsafe {
            let slot = std::ptr::addr_of_mut!((*this).next) as *mut RawGc;
            v(slot);
        }
    }
}

#[test]
fn snapshot_emits_chrome_devtools_format() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Cell>();
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let leaf = heap.alloc(Cell { next: Gc::null() }).unwrap();
    let node = heap.alloc(Cell { next: leaf }).unwrap();
    let _root = scope.local(node);

    let mut bytes = Vec::new();
    write_heap_snapshot(&heap, &mut bytes).expect("write snapshot");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");

    // Header.
    let meta = &parsed["snapshot"]["meta"];
    assert!(meta["node_fields"].is_array(), "missing node_fields");
    assert!(meta["edge_fields"].is_array(), "missing edge_fields");

    let node_count = parsed["snapshot"]["node_count"]
        .as_u64()
        .expect("node_count");
    assert!(node_count >= 2, "expected ≥2 nodes (leaf + node)");

    // Node array length must equal `node_count * 7`.
    let nodes = parsed["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len() as u64, node_count * 7);

    // Edges array is multiple of 3.
    let edges = parsed["edges"].as_array().expect("edges array");
    assert!(
        edges.len().is_multiple_of(3),
        "edges count not a multiple of 3"
    );
    assert_eq!(edges.len(), 3, "node should point to its leaf");
    let target = edges[2].as_u64().expect("edge target");
    assert_eq!(target % 7, 0, "edge target must align to a node row");
    assert!(target < nodes.len() as u64, "edge target must be live");

    // String table is non-empty (canonical "" plus our names).
    let strings = parsed["strings"].as_array().expect("strings array");
    assert!(strings.len() >= 2, "string table too small");
}
