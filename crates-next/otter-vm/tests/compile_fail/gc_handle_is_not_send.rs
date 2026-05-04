//! `Gc<T>` must not be `Send` — capturing one in a `Send + 'static`
//! closure (the shape `tokio::spawn` requires) must fail to
//! compile.
//!
//! Spec: ADR-0005 §3 / task 76A.5.

fn assert_send<T: Send>(_t: T) {}

fn main() {
    let handle: otter_gc::Gc<()> = otter_gc::Gc::null();
    assert_send(handle);
}
