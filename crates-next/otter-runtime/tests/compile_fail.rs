//! Compile-fail fixtures for the public Tokio host-future boundary.
//!
//! Task 85 requires the runtime facade to be `Send + Sync` while VM
//! and GC internals remain isolate-local. These fixtures exercise the
//! actual `tokio::spawn` shape public async host work will use.

#[test]
fn tokio_spawn_rejects_vm_and_gc_handles() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/tokio_spawn_value_is_not_send.rs");
    t.compile_fail("tests/compile_fail/tokio_spawn_frame_is_not_send.rs");
    t.compile_fail("tests/compile_fail/tokio_spawn_gc_handle_is_not_send.rs");
    t.compile_fail("tests/compile_fail/tokio_spawn_local_is_not_send.rs");
    t.compile_fail("tests/compile_fail/tokio_spawn_native_ctx_is_not_send.rs");
}
