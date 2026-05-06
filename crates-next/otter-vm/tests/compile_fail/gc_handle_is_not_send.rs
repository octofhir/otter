//! `Gc<T>` must not be `Send` — capturing one in a `Send + 'static`
//! closure (the shape `tokio::spawn` requires) must fail to
//! compile.


fn assert_send<T: Send>(_t: T) {}

fn main() {
    let handle: otter_gc::Gc<()> = otter_gc::Gc::null();
    assert_send(handle);
}
