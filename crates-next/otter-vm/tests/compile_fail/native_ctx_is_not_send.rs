//! `NativeCtx` is isolate-local and must not satisfy `Send`.

fn assert_send<T: Send>() {}

fn main() {
    assert_send::<otter_vm::NativeCtx<'static>>();
}
