//! The unbranded persistent handle type is not part of the public
//! collector API. External persistent roots must use `Root<'iso, T>`.

fn main() {
    let _handle: Option<otter_gc::GlobalHandle<otter_gc::test_support::OpaqueLeaf>> = None;
}
