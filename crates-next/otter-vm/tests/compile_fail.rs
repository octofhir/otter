//! Compile-fail fixtures proving the VM / GC handle types are
//! `!Send + !Sync` and so cannot be captured by Tokio futures.
//!
//! Per ADR-0005 §3 ("Stability and early detection are required
//! features") and task 76A.5, the new-engine VM must reject these
//! shapes at compile time:
//!
//! - capturing `Gc<T>` in `tokio::spawn`;
//! - capturing `Local<'gc, T>` in `tokio::spawn`;
//! - holding a `RuntimeCx<'_>` / `NativeCtx<'_>` across `.await`;
//! - returning an internal `Value` from a `Send + 'static` future.
//!
//! Each fixture under `tests/compile_fail/` is a `.rs` snippet
//! whose `cargo build` must fail — the canonical [`trybuild`]
//! harness checks for the expected error.
//!
//! Spec / source:
//! - [`docs/new-engine/adr/0005-async-runtime-binding.md`] §6.
//! - [`docs/new-engine/tasks/76a-runtime-binding-explicit-context.md`] §5.

#[test]
fn compile_fail_send_sync_invariants() {
    // The fixtures below establish three load-bearing properties:
    //   1. Raw `Gc<T>` handles are `!Send` (compressed.rs invariants).
    //   2. `Local<'gc, T>` is `!Send` (handle.rs lifetime contract).
    //   3. `GcHeap` itself is `!Send` (single-mutator-per-isolate).
    //   4. `Value` and parked `Frame` state are `!Send`, so async
    //      host futures cannot capture JS payloads directly.
    //
    // Together these guarantee that no VM/GC handle can leak into a
    // `Send + 'static` future — the shape `tokio::spawn` requires —
    // since `Interpreter` transitively contains `GcHeap` and is
    // therefore `!Send` (also enforced by the static_assertions in
    // `crate::lib`).
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/gc_handle_is_not_send.rs");
    t.compile_fail("tests/compile_fail/local_is_not_send.rs");
    t.compile_fail("tests/compile_fail/heap_is_not_send.rs");
    t.compile_fail("tests/compile_fail/native_ctx_is_not_send.rs");
    t.compile_fail("tests/compile_fail/value_is_not_send.rs");
    t.compile_fail("tests/compile_fail/frame_is_not_send.rs");
}

#[test]
fn compile_fail_branded_gc_session_invariants() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/branded_root_cross_isolate_rejected.rs");
    t.compile_fail("tests/compile_fail/branded_weak_cross_isolate_rejected.rs");
    t.compile_fail("tests/compile_fail/branded_session_across_await_rejected.rs");
    t.compile_fail("tests/compile_fail/branded_root_native_closure_rejected.rs");
}
