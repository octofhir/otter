//! `HeapSnapshot` records every parent → child edge in a 3-node
//! cycle and `retained_size(root)` covers all three nodes when
//! the cycle's only entry point is `root`.
//!
//! # See also
//!
//! - GC architecture plan §7 ("Leak diagnosis"), §1.2 NF6.
//! - Task 74 — GC stats, heap snapshot, retained-size walker.

use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{Gc, GcHeader, GcHeap, HandleScope, RawGc};

#[derive(Debug)]
struct Node {
    next: Gc<Node>,
}

impl Traceable for Node {
    const TYPE_TAG: u8 = 0x41;
    unsafe fn trace_slots(this: *mut Self, v: &mut SlotVisitor<'_>) {
        unsafe {
            let slot = std::ptr::addr_of_mut!((*this).next) as *mut RawGc;
            v(slot);
        }
    }
}

/// Mutate `parent.next` to point at `child` and inform the
/// barrier of the store. Used to close the cycle.
fn link(heap: &mut GcHeap, parent: Gc<Node>, child: Gc<Node>) {
    // SAFETY: parent / child are live allocations carved from
    // the heap's own cage; the slot lives at offset
    // sizeof::<GcHeader>() inside parent's payload.
    unsafe {
        let payload =
            (parent.as_header_ptr() as *mut u8).add(std::mem::size_of::<GcHeader>()) as *mut Node;
        let slot = std::ptr::addr_of_mut!((*payload).next);
        *slot = child;
        heap.write_barrier(parent, slot, child);
    }
}

#[test]
fn snapshot_walks_cycle_and_attributes_retained_size() {
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Node>();

    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let a = heap.alloc(Node { next: Gc::null() }).unwrap();
    let b = heap.alloc(Node { next: Gc::null() }).unwrap();
    let c = heap.alloc(Node { next: Gc::null() }).unwrap();
    link(&mut heap, a, b);
    link(&mut heap, b, c);
    link(&mut heap, c, a);
    let _ra = scope.local(a);
    let _rb = scope.local(b);
    let _rc = scope.local(c);

    let snap = heap.snapshot(&[a.raw()]);

    assert_eq!(snap.objects.len(), 3, "expected three live nodes");
    assert_eq!(snap.edges.len(), 3, "expected three edges in cycle");

    let edge_set: std::collections::HashSet<(u32, u32)> =
        snap.edges.iter().map(|(p, c)| (p.0, c.0)).collect();
    assert!(edge_set.contains(&(a.raw().0, b.raw().0)));
    assert!(edge_set.contains(&(b.raw().0, c.raw().0)));
    assert!(edge_set.contains(&(c.raw().0, a.raw().0)));

    let total_self_size: usize = snap.objects.iter().map(|o| o.self_size).sum();
    let retained = snap.retained_size(a.raw());
    assert_eq!(
        retained, total_self_size,
        "single-root cycle: retained_size(root) == sum of all self_size"
    );

    let by_type = snap.group_by_type();
    assert_eq!(by_type[Node::TYPE_TAG as usize], total_self_size);
}
