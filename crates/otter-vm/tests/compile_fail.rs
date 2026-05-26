//! Compile-fail fixtures proving the VM / GC handle types are
//! `!Send + !Sync` and so cannot be captured by Tokio futures.
//!
//! The active VM must reject these shapes at compile time:
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
//! See the mdBook event-loop and GC API chapters for the runtime boundary.

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
fn compile_fail_pelt_derive_invariants() {
    // The fixtures below establish two load-bearing properties of
    // `#[derive(Pelt)]`:
    //   1. Missing `#[pelt(tag = …)]` is a compile error — two bodies
    //      cannot silently share the same `Traceable::TYPE_TAG` slot.
    //   2. A field whose type does not implement
    //      `otter_vm::pelt::PeltField` fails at the field's span —
    //      authors must either add the impl or annotate the field
    //      with `#[pelt(skip)]`.
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/pelt_missing_tag.rs");
    t.compile_fail("tests/compile_fail/pelt_untraceable_field.rs");
}

#[test]
fn compile_fail_branded_gc_session_invariants() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/branded_root_cross_isolate_rejected.rs");
    t.compile_fail("tests/compile_fail/branded_weak_cross_isolate_rejected.rs");
    t.compile_fail("tests/compile_fail/branded_session_across_await_rejected.rs");
    t.compile_fail("tests/compile_fail/branded_root_native_closure_rejected.rs");
    t.compile_fail("tests/compile_fail/native_closure_gc_capture_rejected.rs");
    t.compile_fail("tests/compile_fail/unbranded_global_handle_creation_rejected.rs");
    t.compile_fail("tests/compile_fail/unbranded_global_handle_type_rejected.rs");
    t.compile_fail("tests/compile_fail/raw_write_barrier_rejected.rs");
    t.compile_fail("tests/compile_fail/root_raw_gc_import_rejected.rs");
    t.compile_fail("tests/compile_fail/root_trace_table_import_rejected.rs");
}
