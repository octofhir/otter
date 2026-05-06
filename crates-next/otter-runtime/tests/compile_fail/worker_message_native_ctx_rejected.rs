//! Worker messages must not carry native mutator contexts.

fn send_native_ctx(ctx: otter_vm::NativeCtx<'_>) {
    let worker = otter_runtime::Worker::new().unwrap();
    worker.accepts_message(&ctx);
}

fn main() {
    let _ = send_native_ctx;
}
