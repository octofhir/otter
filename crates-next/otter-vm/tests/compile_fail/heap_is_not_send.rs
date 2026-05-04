//! `GcHeap` must not be `Send` (single-mutator-per-isolate, per
//! ECMA-262 §16.6 Agents and ADR-0005 §3). A future that captures
//! `&mut GcHeap` and tries to satisfy `Send` must fail.

fn assert_send<T: Send>(_t: T) {}

fn main() {
    let heap = otter_gc::GcHeap::new().unwrap();
    assert_send(heap);
}
