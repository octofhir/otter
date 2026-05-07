use otter_gc::raw::RawGc;

fn bypass(
    heap: &mut otter_gc::GcHeap,
    parent: otter_gc::Gc<otter_gc::test_support::OpaqueLeaf>,
    slot: *mut RawGc,
    child: RawGc,
) {
    heap.write_barrier_raw(parent, slot, child);
}

fn main() {}
