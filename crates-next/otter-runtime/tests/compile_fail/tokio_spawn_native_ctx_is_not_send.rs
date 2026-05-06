//! Native contexts are mutator-turn borrows and must not cross Tokio
//! worker boundaries.

fn spawn_native_ctx<'rt>(ctx: otter_vm::NativeCtx<'rt>) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        std::hint::black_box(ctx);
    });
}

fn main() {
    let _ = spawn_native_ctx;
}
