//! Compile-fail fixtures for the public Tokio host-future boundary.
//!
//! Runtime host handles are `Send + Sync` while VM and GC internals
//! remain isolate-local. These fixtures exercise the actual
//! `tokio::spawn` and worker-message boundary shapes public async host
//! work will use.

#[test]
fn tokio_spawn_rejects_vm_and_gc_handles() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/tokio_spawn_value_is_not_send.rs");
    t.compile_fail("tests/compile_fail/tokio_spawn_frame_is_not_send.rs");
    t.compile_fail("tests/compile_fail/tokio_spawn_gc_handle_is_not_send.rs");
    t.compile_fail("tests/compile_fail/tokio_spawn_local_is_not_send.rs");
    t.compile_fail("tests/compile_fail/tokio_spawn_native_ctx_is_not_send.rs");
}

#[test]
fn worker_message_boundary_rejects_vm_and_gc_handles() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/worker_message_value_rejected.rs");
    t.compile_fail("tests/compile_fail/worker_message_gc_handle_rejected.rs");
    t.compile_fail("tests/compile_fail/worker_message_local_rejected.rs");
    t.compile_fail("tests/compile_fail/worker_message_native_ctx_rejected.rs");
    t.compile_fail("tests/compile_fail/worker_message_branded_root_rejected.rs");
    t.compile_fail("tests/compile_fail/worker_message_branded_weak_rejected.rs");
    t.compile_fail("tests/compile_fail/worker_message_gc_session_rejected.rs");
    t.compile_fail("tests/compile_fail/runtime_raw_heap_access_rejected.rs");
}

#[test]
fn runtime_hooks_reject_non_send_sync_state() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/runtime_hook_non_send_sync_rejected.rs");
}

#[test]
fn generated_bindings_compile_through_hidden_support_boundary() {
    let t = trybuild::TestCases::new();
    t.pass("tests/compile_pass/macro_support_boundary.rs");
}

#[test]
fn untraced_host_payload_requires_explicit_marker() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/untraced_host_value_slot_rejected.rs");
}
